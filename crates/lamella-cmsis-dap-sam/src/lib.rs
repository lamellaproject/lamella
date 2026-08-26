//! Microchip SAM (Atmel SAM) flash programming over a Lamella debug probe.

use lamella_probe_core::{ProbeError, TargetAccess};

const SAMD21_CTRLA: u32 = 0x4100_4000;
const SAMD21_CTRLB: u32 = 0x4100_4004;
const SAMD21_INTFLAG: u32 = 0x4100_4014;
const SAMD21_ADDR: u32 = 0x4100_401c;
const SAMD21_CMDEX: u32 = 0xa500;
const SAMD21_CMD_ER: u32 = 0x02;
const SAMD21_CMD_WP: u32 = 0x04;
const SAMD21_CMD_PBC: u32 = 0x44;
const SAMD21_PAGE: usize = 64;
const SAMD21_ROW: u32 = 256;
const SAMD21_MANW: u32 = 1 << 7;

const DSU_STATUSA: u32 = 0x4100_2001;
const DSU_STATUSA_EXT: u32 = 0x4100_2101;
const DSU_CRSTEXT: u8 = 1 << 1;

/// SAM D21 debug attach beyond the chip-agnostic operations.
pub trait Samd21Debug {
    /// Halts the SAM D21 at its reset vector regardless of what the running firmware does
    /// (an armed watchdog defeats a plain halt request; the DSU reset extension defeats a
    /// plain vector catch): catch armed under held `nRESET`, then the cold-plugging
    /// extension released INTO the catch.
    fn samd21_reset_halt(&mut self) -> Result<(), ProbeError>;

    /// PARKS the SAM D21 so it executes nothing, without needing the vector catch at all:
    /// a probe `nRESET` pulse lands the core in the DSU cold-plugging reset extension
    /// (CRSTEXT holds it; the debug AHB stays live) -- exactly the stopped-core guarantee
    /// flash programming and pin takeover need, and deterministic where the catch is not
    /// (`DEMCR` does not reliably survive this part's external reset). Falls back to a
    /// plain halt for a probe with no reset line wired. The core stays parked/halted until
    /// a reset (`TargetAccess::reset_and_run`) or a CRSTEXT release boots it.
    fn samd21_park(&mut self) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Samd21Debug for A {
    fn samd21_park(&mut self) -> Result<(), ProbeError> {
        for _ in 0..4 {
            let _ = self.set_reset(true);
            let _ = self.set_reset(false);
            for _ in 0..64 {
                if self.read_byte(DSU_STATUSA).map(|s| s & DSU_CRSTEXT != 0).unwrap_or(false) {
                    return Ok(());
                }
            }
            let _ = self.halt();
            if self.is_halted().unwrap_or(false) {
                return Ok(());
            }
        }
        Err(ProbeError::Timeout("park"))
    }

    fn samd21_reset_halt(&mut self) -> Result<(), ProbeError> {
        let _ = self.set_reset(true);
        self.arm_reset_catch()?;
        let _ = self.set_reset(false);
        for _ in 0..256 {
            if self.is_halted().unwrap_or(false) {
                let _ = self.disarm_reset_catch();
                return Ok(());
            }
            if self.read_byte(DSU_STATUSA).map(|s| s & DSU_CRSTEXT != 0).unwrap_or(false) {
                let _ = self.write_byte(DSU_STATUSA, DSU_CRSTEXT);
                let _ = self.write_byte(DSU_STATUSA_EXT, DSU_CRSTEXT);
            }
        }
        let _ = self.disarm_reset_catch();
        Err(ProbeError::Timeout("reset catch"))
    }
}

/// SAM D21 (ATSAMD21) flash programming, added to a CMSIS-DAP [`TargetAccess`] probe. Halt the core before
/// erasing or writing so it is not fetching from flash during the operation.
pub trait Samd21Flash {
    /// Erases the flash row (256 bytes) containing `address`, via the NVMCTRL.
    fn erase_flash_row(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs consecutive 32-bit `words` to flash from `address`, via the NVMCTRL, one 64-byte
    /// page at a time (the rows must already be erased).
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Samd21Flash for A {
    fn erase_flash_row(&mut self, address: u32) -> Result<(), ProbeError> {
        self.write_word(SAMD21_ADDR, (address & !(SAMD21_ROW - 1)) / 2)?;
        samd21_command(self, SAMD21_CMD_ER)
    }

    /// Manual write, per datasheet 22.6.4.3.1: clear the page buffer, fill it through the flash
    /// address space, issue a read-memory barrier, set the page address, then Write-Page.
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        let ctrlb = self.read_word(SAMD21_CTRLB)?;
        self.write_word(SAMD21_CTRLB, ctrlb | SAMD21_MANW)?;
        for (page, chunk) in words.chunks(SAMD21_PAGE / 4).enumerate() {
            let page_addr = address + (page as u32) * SAMD21_PAGE as u32;
            samd21_command(self, SAMD21_CMD_PBC)?;
            self.write_words(page_addr, chunk)?;
            self.read_word(page_addr)?;
            self.write_word(SAMD21_ADDR, page_addr / 2)?;
            samd21_command(self, SAMD21_CMD_WP)?;
        }
        Ok(())
    }
}

/// Issues an NVMCTRL command (CMDEX key + `cmd`) and waits for the controller to be ready.
fn samd21_command<A: TargetAccess>(target: &mut A, cmd: u32) -> Result<(), ProbeError> {
    target.write_word(SAMD21_CTRLA, SAMD21_CMDEX | cmd)?;
    for _ in 0..1000 {
        if target.read_word(SAMD21_INTFLAG)? & 1 != 0 {
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("SAMD21 flash controller"))
}

/// SAM DSU `DID` -- the part's own identification word, at DSU base + 0x18.
pub const SAM_DSU_DID: u32 = 0x4100_2018;
/// The same `DID` through the DSU's EXTERNAL view.
///
/// The DSU's first 0x100 bytes are the internal address range and the next 0x100 mirror them as
/// the external range (SAM D11 12.9, 12.11.2.2). A part locked by the NVMCTRL security bit
/// discards debug-adapter accesses below 0x100 with an error response, so the external view is the
/// one that still answers on a protected part -- and the same word on an unprotected one.
pub const SAM_DSU_DID_EXTERNAL: u32 = 0x4100_2118;
/// SAM NVMCTRL `PARAM` (+0x08): `NVMP` pages in [15:0], `PSZ` page-size code in [18:16].
pub const SAM_NVMCTRL_PARAM: u32 = 0x4100_4008;
/// The SAM D10/D11/D21 erase unit is a row of FOUR pages (SAM D10 and D11 21.6.3, SAM D21 22.6.3),
/// so a row is four times whatever page size [`SamFlashGeometry`] reports -- 256 bytes on all three
/// parts today, since each has 64-byte pages.
pub const SAMD21_PAGES_PER_ROW: u32 = 4;
/// `DIE` and `REVISION` masked out of a `DID`: what is left names the part, not the die spin.
const DID_PART_KEY: u32 = 0xffff_00ff;

/// A SAM part as its own silicon reports it: the fields of the DSU `DID` register.
///
/// This is the only identification that comes from the die. A kit label, a USB product id and an
/// operator's expectation can each name a different board than the one the probe is wired to: two
/// SAM D21 kits share EDBG product id 0x2169, three Xplained Pro kits share 0x2111, an EDBG serial
/// names the DEBUGGER, and a Cortex-M0-class DP IDCODE is answered by parts from two vendors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SamDeviceId {
    /// The register as read.
    pub raw: u32,
    /// `PROCESSOR[31:28]`: 0x0 Cortex-M0, 0x1 Cortex-M0+, 0x2 Cortex-M3, 0x3 Cortex-M4.
    pub processor: u8,
    /// `FAMILY[27:23]`: 0x0 general purpose, 0x1 PicoPower.
    pub family: u8,
    /// `SERIES[21:16]`: the product series within the family.
    pub series: u8,
    /// `DIE[15:12]`: the die within the family.
    pub die: u8,
    /// `REVISION[11:8]`: 0x0 = rev A, 0x1 = rev B, and so on.
    pub revision: u8,
    /// `DEVSEL[7:0]`: flash density, pin count and variant -- the part within the series.
    pub devsel: u8,
}

impl SamDeviceId {
    /// Splits a `DID` word into its fields.
    pub fn decode(raw: u32) -> Self {
        Self {
            raw,
            processor: ((raw >> 28) & 0xf) as u8,
            family: ((raw >> 23) & 0x1f) as u8,
            series: ((raw >> 16) & 0x3f) as u8,
            die: ((raw >> 12) & 0xf) as u8,
            revision: ((raw >> 8) & 0xf) as u8,
            devsel: (raw & 0xff) as u8,
        }
    }

    /// The Cortex-M core `PROCESSOR` names, or `None` for a code no datasheet read for this table
    /// tabulates.
    pub fn core(&self) -> Option<&'static str> {
        match self.processor {
            0x0 => Some("Cortex-M0"),
            0x1 => Some("Cortex-M0+"),
            0x2 => Some("Cortex-M3"),
            0x3 => Some("Cortex-M4"),
            0x6 => Some("Cortex-M4F"),
            _ => None,
        }
    }

    /// The die revision as its letter: `REVISION` 0 is rev A.
    pub fn revision_letter(&self) -> char {
        (b'A' + self.revision.min(25)) as char
    }

    /// The exact part, for the rows a document or a measurement sources.
    ///
    /// `None` is not a failure and not an unknown part: it means this table has no sourced row for
    /// that `DEVSEL`, while the fields above still give the family, series, die and revision.
    pub fn part(&self) -> Option<&'static str> {
        match self.raw & DID_PART_KEY {
            0x1003_0000 => Some("ATSAMD11D14AM (24-pin QFN)"),
            0x1003_0003 => Some("ATSAMD11D14ASS (20-pin SOIC)"),
            0x1003_0006 => Some("ATSAMD11C14A (14-pin SOIC)"),
            0x1003_0009 => Some("ATSAMD11D14AU (20-ball WLCSP)"),
            0x1002_0000 => Some("ATSAMD10D14AM (24-pin QFN)"),
            0x1002_0001 => Some("ATSAMD10D13AM (24-pin QFN)"),
            0x1002_0003 => Some("ATSAMD10D14ASS (20-pin SOIC)"),
            0x1002_0004 => Some("ATSAMD10D13ASS (20-pin SOIC)"),
            0x1002_0006 => Some("ATSAMD10C14A (14-pin SOIC)"),
            0x1002_0007 => Some("ATSAMD10C13A (14-pin SOIC)"),
            0x1002_0009 => Some("ATSAMD10D14AU (20-ball WLCSP)"),
            0x1001_0000 => Some("ATSAMD21J18A"),
            _ => None,
        }
    }

    /// Whether this part's flash controller is the one the [`Samd21Flash`] routines drive: the
    /// SAM D10 (series 0x2), SAM D11 (0x3) and SAM D21 (0x1) NVMCTRL agree register for register --
    /// base, offsets, the 0xA5 `CMDEX` key, the command codes, the half-word `ADDR` and the
    /// four-page row (Atmel-42242H and Atmel-42363H ch.21 against DS40001882 ch.22).
    ///
    /// Read the series rather than trusting the family to be uniform: the SAM D5x/E5x NVMCTRL puts
    /// its command register where the D21 puts its configuration register. A series absent from
    /// this list is not a claim that its controller differs -- it is one nobody has read a
    /// datasheet for, which is the same refusal for a different reason.
    pub fn drives_samd21_nvmctrl(&self) -> bool {
        self.processor == 0x1 && self.family == 0x0 && matches!(self.series, 0x1 | 0x2 | 0x3)
    }

    /// Whether this part's flash controller is the one the [`Same54Flash`] routines drive: the
    /// SAM D5x/E5x NVMCTRL, which erases by 8 KiB block and puts its command register where the
    /// D21 puts its configuration register.
    ///
    /// **Measured, not tabulated.** DS60001507 documents what the `DID` FIELDS mean but publishes
    /// no table of their values, so this is the identity read off the SAM E54 Xplained Pro:
    /// `DID 0x61840300`, processor `0x6`, family `0x3`, series `0x4`. The other members of
    /// the family (D51, E51, E53) are not claimed here -- not because their controller is thought
    /// to differ, but because nobody has read one off a part, which is the same discipline the
    /// D21 `DEVSEL` rows follow.
    pub fn drives_same54_nvmctrl(&self) -> bool {
        self.processor == 0x6 && self.family == 0x3 && self.series == 0x4
    }

    /// Which flash routine in this crate drives this part, if either does.
    ///
    /// **A part absent from both is not a part this crate cannot flash** -- it is one whose
    /// controller nobody has read a datasheet for. The distinction matters because the two answers
    /// call for opposite next steps, and a tool that prints the first when it means the second
    /// sends a reader looking for a document that is already on the shelf.
    pub fn flash_routine(&self) -> Option<&'static str> {
        if self.drives_samd21_nvmctrl() {
            return Some("Samd21Flash");
        }
        if self.drives_same54_nvmctrl() {
            return Some("Same54Flash");
        }
        None
    }
}

/// The flash geometry a SAM part reports about itself through NVMCTRL `PARAM`.
///
/// A probe-side tool has no linker script and no running firmware to ask how big the part in front
/// of it is, and a table of part sizes inside a host tool is a second spelling that nothing checks.
/// `PARAM` is the part's own answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SamFlashGeometry {
    /// `NVMP[15:0]`: pages in the NVM main address space.
    pub pages: u32,
    /// `8 << PSZ`: the page size in bytes, which is the write granularity.
    pub page_bytes: u32,
}

impl SamFlashGeometry {
    /// Decodes a `PARAM` read.
    pub fn decode(param: u32) -> Self {
        Self { pages: param & 0xffff, page_bytes: 8 << ((param >> 16) & 0x7) }
    }

    /// The size of the main array, which is what an image has to fit inside.
    pub fn flash_bytes(&self) -> u32 {
        self.pages * self.page_bytes
    }

    /// The SAM D11/D21 erase granularity: four pages.
    pub fn samd21_row_bytes(&self) -> u32 {
        self.page_bytes * SAMD21_PAGES_PER_ROW
    }
}

/// Reading a SAM part's identity and flash geometry off the part itself, over whatever probe
/// reached it. Both reads are non-destructive and neither needs a halt.
pub trait SamIdentify {
    /// Reads and decodes the DSU `DID`.
    ///
    /// An all-zero or all-ones word is refused rather than decoded. Those are what a MEM-AP that
    /// is not reaching the part returns, and every decoded field would then be a confident zero --
    /// a tool whose answer is an identity has to show it can see one. Such a read is retried
    /// against [`SAM_DSU_DID_EXTERNAL`] before it is refused.
    ///
    /// A part locked by the NVMCTRL security bit does not answer zero on the internal view: it
    /// takes the access down with an error response, which arrives here as a transport error
    /// rather than a refusal, and is cleared by the caller before retrying at the external view.
    fn sam_device_id(&mut self) -> Result<SamDeviceId, ProbeError>;

    /// Reads NVMCTRL `PARAM` and decodes the flash geometry. A zero page count is refused for the
    /// same reason: a part with no flash is not what a working read looks like.
    fn sam_flash_geometry(&mut self) -> Result<SamFlashGeometry, ProbeError>;
}

impl<A: TargetAccess> SamIdentify for A {
    fn sam_device_id(&mut self) -> Result<SamDeviceId, ProbeError> {
        let mut raw = self.read_word(SAM_DSU_DID)?;
        if raw == 0 || raw == u32::MAX {
            raw = self.read_word(SAM_DSU_DID_EXTERNAL)?;
        }
        if raw == 0 || raw == u32::MAX {
            return Err(ProbeError::Device(
                "DSU DID reads all zeros or all ones in both DSU views -- no part answering",
            ));
        }
        Ok(SamDeviceId::decode(raw))
    }

    fn sam_flash_geometry(&mut self) -> Result<SamFlashGeometry, ProbeError> {
        let geometry = SamFlashGeometry::decode(self.read_word(SAM_NVMCTRL_PARAM)?);
        if geometry.pages == 0 {
            return Err(ProbeError::Device("NVMCTRL PARAM reports zero pages -- no NVMCTRL answering"));
        }
        Ok(geometry)
    }
}

const SAME54_CTRLA: u32 = 0x4100_4000;
const SAME54_CTRLB: u32 = 0x4100_4004;
const SAME54_INTFLAG: u32 = 0x4100_4010;
const SAME54_ADDR: u32 = 0x4100_4014;
const SAME54_CMDEX: u32 = 0xa500;
const SAME54_CMD_EB: u32 = 0x01;
const SAME54_CMD_WP: u32 = 0x03;
const SAME54_CMD_PBC: u32 = 0x15;
const SAME54_PAGE: usize = 512;
/// The SAM D5x/E5x erase granularity: one 8 KiB block (DS60001507, NVMCTRL).
pub const SAME54_BLOCK: u32 = 8192;
const SAME54_STATUS_READY: u32 = 1 << 16;
const SAME54_WMODE_MASK: u32 = 0b11 << 4;

/// SAM D5x/E5x (ATSAME54, ATSAMD51, ...) flash programming, added to a CMSIS-DAP [`TargetAccess`]
/// probe. Halt the core before erasing or writing so it is not fetching from flash during
/// the operation.
pub trait Same54Flash {
    /// Erases the flash block (8 KiB) containing `address`, via the NVMCTRL.
    fn erase_flash_block(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs consecutive 32-bit `words` to flash from `address`, via the NVMCTRL, one
    /// 512-byte page at a time (the blocks must already be erased).
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Same54Flash for A {
    fn erase_flash_block(&mut self, address: u32) -> Result<(), ProbeError> {
        same54_ready(self)?;
        self.write_word(SAME54_ADDR, address & !(SAME54_BLOCK - 1))?;
        same54_command(self, SAME54_CMD_EB)
    }

    /// Manual write, per the datasheet's manual-page-write procedure: set WMODE = MAN,
    /// then per page: clear the page buffer, fill it through the flash address space,
    /// set the page address, Write-Page.
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        let ctrla = self.read_word(SAME54_CTRLA)?;
        self.write_word(SAME54_CTRLA, ctrla & !SAME54_WMODE_MASK)?;
        for (page, chunk) in words.chunks(SAME54_PAGE / 4).enumerate() {
            let page_addr = address + (page as u32) * SAME54_PAGE as u32;
            same54_ready(self)?;
            same54_command(self, SAME54_CMD_PBC)?;
            self.write_words(page_addr, chunk)?;
            self.write_word(SAME54_ADDR, page_addr)?;
            same54_command(self, SAME54_CMD_WP)?;
        }
        Ok(())
    }
}

/// Polls STATUS.READY (the controller accepts a new command).
fn same54_ready<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    for _ in 0..1000 {
        if target.read_word(SAME54_INTFLAG)? & SAME54_STATUS_READY != 0 {
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("SAME54 flash controller ready"))
}

/// Issues an NVMCTRL command (CMDEX key + `cmd` into CTRLB) and waits for ready.
fn same54_command<A: TargetAccess>(target: &mut A, cmd: u32) -> Result<(), ProbeError> {
    target.write_word(SAME54_CTRLB, SAME54_CMDEX | cmd)?;
    same54_ready(target)
}

/// EEFC0 user-interface base: the controller for the plane in the 0x00400000 window.
pub const SAM4S_EEFC0: u32 = 0x400e_0a00;
/// EEFC1 user-interface base: the second controller of the dual-plane SAM4SD16/SD32.
pub const SAM4S_EEFC1: u32 = 0x400e_0c00;
/// The plane-0 flash window (also mirrored at 0x0 when GPNVM1 selects flash boot).
pub const SAM4S_FLASH0_BASE: u32 = 0x0040_0000;
/// SAM4SD32 second plane window (1 MB planes); the SAM4SD16's is at 0x0048_0000.
pub const SAM4S_FLASH1_BASE: u32 = 0x0050_0000;
/// SAM4S flash page size in bytes (also the EEFC write granularity).
pub const SAM4S_PAGE: usize = 512;
/// GPNVM bit index of the security bit (set = the debug port is locked out).
pub const SAM4S_GPNVM_SECURITY: u32 = 0;
/// GPNVM bit index of the boot-mode bit (set = boot flash, clear = boot the SAM-BA ROM).
pub const SAM4S_GPNVM_BOOT_FLASH: u32 = 1;
/// GPNVM bit index of the SAM4SD16/SD32 plane swap (set = flash 1 in the 0x00400000 window).
pub const SAM4S_GPNVM_PLANE_SWAP: u32 = 2;

const SAM4S_FCR: u32 = 0x04;
const SAM4S_FSR: u32 = 0x08;
const SAM4S_FRR: u32 = 0x0c;
const SAM4S_FKEY: u32 = 0x5a << 24;
const SAM4S_CMD_GETD: u32 = 0x00;
const SAM4S_CMD_WP: u32 = 0x01;
const SAM4S_CMD_EPA: u32 = 0x07;
const SAM4S_CMD_CLB: u32 = 0x09;
const SAM4S_CMD_GLB: u32 = 0x0a;
const SAM4S_CMD_SGPB: u32 = 0x0b;
const SAM4S_CMD_CGPB: u32 = 0x0c;
const SAM4S_CMD_GGPB: u32 = 0x0d;
const SAM4S_CMD_STUI: u32 = 0x0e;
const SAM4S_CMD_SPUI: u32 = 0x0f;
const SAM4S_FSR_FRDY: u32 = 1 << 0;
const SAM4S_FSR_FCMDE: u32 = 1 << 1;
const SAM4S_FSR_FLOCKE: u32 = 1 << 2;
const SAM4S_FSR_FLERR: u32 = 1 << 3;
/// EPA's FARG[1:0] = 1 selects 8 pages (4 KiB) -- the one block size legal in BOTH the
/// small 8 KB sectors (which forbid 16/32) and the 48/64 KB sectors (which forbid 4).
const SAM4S_EPA_8_PAGES: u32 = 1;
/// Pages per [`Sam4sFlash::sam4s_erase_pages8`] erase (the EPA 8-page block).
pub const SAM4S_ERASE_PAGES: u32 = 8;

/// The first words of the flash descriptor a GETD command returns -- the live geometry
/// cross-check before any erase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sam4sFlashDescriptor {
    /// FL_ID: flash interface description.
    pub interface: u32,
    /// FL_SIZE: plane size in bytes.
    pub size: u32,
    /// FL_PAGE_SIZE: page size in bytes (512 on the SAM4S).
    pub page_size: u32,
    /// FL_NB_PLANE: number of planes this controller fronts.
    pub planes: u32,
}

/// SAM4S (ATSAM4S / SAM4SD dual-plane) flash programming, added to a CMSIS-DAP [`TargetAccess`]
/// probe. `eefc` selects the controller ([`SAM4S_EEFC0`] / [`SAM4S_EEFC1`]); page numbers
/// are relative to that controller's plane. Halt the core before erasing or writing.
pub trait Sam4sFlash {
    /// Erases 8 pages (4 KiB) starting at `first_page` (a multiple of 8), via EPA.
    fn sam4s_erase_pages8(&mut self, eefc: u32, first_page: u32) -> Result<(), ProbeError>;
    /// Programs `words` into consecutive pages starting at `first_page` of the plane
    /// mapped at `plane_base` (already erased). Each page's latch buffer is filled
    /// completely -- the tail beyond `words` is padded with the erased value -- because
    /// ascending full-buffer fills are the documented procedure and the partial-fill
    /// erratum's workaround.
    fn sam4s_write_flash(
        &mut self,
        eefc: u32,
        plane_base: u32,
        first_page: u32,
        words: &[u32],
    ) -> Result<(), ProbeError>;
    /// The plane's 128 lock bits (bit n = lock region n of 16 pages), via GLB.
    fn sam4s_lock_bits(&mut self, eefc: u32) -> Result<[u32; 4], ProbeError>;
    /// Clears the lock bit of the region containing `page`, via CLB.
    fn sam4s_clear_lock(&mut self, eefc: u32, page: u32) -> Result<(), ProbeError>;
    /// The GPNVM bits (bit 0 = security, 1 = boot mode, 2 = plane swap), via EEFC0 GGPB.
    fn sam4s_gpnvm_bits(&mut self) -> Result<u32, ProbeError>;
    /// Sets GPNVM bit `bit`, via EEFC0 SGPB.
    fn sam4s_set_gpnvm(&mut self, bit: u32) -> Result<(), ProbeError>;
    /// Clears GPNVM bit `bit`, via EEFC0 CGPB.
    fn sam4s_clear_gpnvm(&mut self, bit: u32) -> Result<(), ProbeError>;
    /// The controller's flash descriptor (GETD): the live geometry cross-check.
    fn sam4s_flash_descriptor(&mut self, eefc: u32) -> Result<Sam4sFlashDescriptor, ProbeError>;
    /// The 128-bit factory unique identifier, via STUI/SPUI. While the sequence is open the
    /// plane's first words read as the identifier area, so the core must be halted.
    fn sam4s_unique_id(&mut self, eefc: u32, plane_base: u32) -> Result<[u32; 4], ProbeError>;
}

impl<A: TargetAccess> Sam4sFlash for A {
    fn sam4s_erase_pages8(&mut self, eefc: u32, first_page: u32) -> Result<(), ProbeError> {
        assert!(first_page % SAM4S_ERASE_PAGES == 0, "EPA start page must be 8-aligned");
        sam4s_command(self, eefc, SAM4S_CMD_EPA, first_page | SAM4S_EPA_8_PAGES)
    }

    fn sam4s_write_flash(
        &mut self,
        eefc: u32,
        plane_base: u32,
        first_page: u32,
        words: &[u32],
    ) -> Result<(), ProbeError> {
        const PAGE_WORDS: usize = SAM4S_PAGE / 4;
        for (index, chunk) in words.chunks(PAGE_WORDS).enumerate() {
            let page = first_page + index as u32;
            let page_addr = plane_base + page * SAM4S_PAGE as u32;
            if chunk.len() == PAGE_WORDS {
                self.write_words(page_addr, chunk)?;
            } else {
                let mut full = [0xffff_ffffu32; PAGE_WORDS];
                full[..chunk.len()].copy_from_slice(chunk);
                self.write_words(page_addr, &full)?;
            }
            sam4s_command(self, eefc, SAM4S_CMD_WP, page)?;
        }
        Ok(())
    }

    fn sam4s_lock_bits(&mut self, eefc: u32) -> Result<[u32; 4], ProbeError> {
        sam4s_command(self, eefc, SAM4S_CMD_GLB, 0)?;
        let mut bits = [0u32; 4];
        for word in &mut bits {
            *word = self.read_word(eefc + SAM4S_FRR)?;
        }
        Ok(bits)
    }

    fn sam4s_clear_lock(&mut self, eefc: u32, page: u32) -> Result<(), ProbeError> {
        sam4s_command(self, eefc, SAM4S_CMD_CLB, page)?;
        sam4s_post_bit_write_dummy_read(self)
    }

    fn sam4s_gpnvm_bits(&mut self) -> Result<u32, ProbeError> {
        sam4s_command(self, SAM4S_EEFC0, SAM4S_CMD_GGPB, 0)?;
        self.read_word(SAM4S_EEFC0 + SAM4S_FRR)
    }

    fn sam4s_set_gpnvm(&mut self, bit: u32) -> Result<(), ProbeError> {
        sam4s_command(self, SAM4S_EEFC0, SAM4S_CMD_SGPB, bit)?;
        sam4s_post_bit_write_dummy_read(self)
    }

    fn sam4s_clear_gpnvm(&mut self, bit: u32) -> Result<(), ProbeError> {
        sam4s_command(self, SAM4S_EEFC0, SAM4S_CMD_CGPB, bit)?;
        sam4s_post_bit_write_dummy_read(self)
    }

    fn sam4s_flash_descriptor(&mut self, eefc: u32) -> Result<Sam4sFlashDescriptor, ProbeError> {
        sam4s_command(self, eefc, SAM4S_CMD_GETD, 0)?;
        Ok(Sam4sFlashDescriptor {
            interface: self.read_word(eefc + SAM4S_FRR)?,
            size: self.read_word(eefc + SAM4S_FRR)?,
            page_size: self.read_word(eefc + SAM4S_FRR)?,
            planes: self.read_word(eefc + SAM4S_FRR)?,
        })
    }

    fn sam4s_unique_id(&mut self, eefc: u32, plane_base: u32) -> Result<[u32; 4], ProbeError> {
        self.write_word(eefc + SAM4S_FCR, SAM4S_FKEY | SAM4S_CMD_STUI)?;
        for _ in 0..1000 {
            if self.read_word(eefc + SAM4S_FSR)? & SAM4S_FSR_FRDY == 0 {
                let mut id = [0u32; 4];
                for (index, word) in id.iter_mut().enumerate() {
                    *word = self.read_word(plane_base + index as u32 * 4)?;
                }
                sam4s_command(self, eefc, SAM4S_CMD_SPUI, 0)?;
                return Ok(id);
            }
        }
        Err(ProbeError::Timeout("SAM4S unique-identifier area (FRDY fall after STUI)"))
    }
}

/// Issues one EEFC command (key | arg | cmd into FCR) and waits for FRDY, mapping the FSR
/// error flags: FCMDE = bad key/command, FLOCKE = the command hit a locked region and was
/// refused, FLERR = the flash's own erase/write verify failed.
fn sam4s_command<A: TargetAccess>(
    target: &mut A,
    eefc: u32,
    cmd: u32,
    arg: u32,
) -> Result<(), ProbeError> {
    target.write_word(eefc + SAM4S_FCR, SAM4S_FKEY | (arg << 8) | cmd)?;
    for _ in 0..4000 {
        let fsr = target.read_word(eefc + SAM4S_FSR)?;
        if fsr & SAM4S_FSR_FRDY != 0 {
            if fsr & SAM4S_FSR_FCMDE != 0 {
                return Err(ProbeError::Device("SAM4S EEFC command error (FCMDE)"));
            }
            if fsr & SAM4S_FSR_FLOCKE != 0 {
                return Err(ProbeError::Device("SAM4S EEFC lock violation (FLOCKE)"));
            }
            if fsr & SAM4S_FSR_FLERR != 0 {
                return Err(ProbeError::Device("SAM4S EEFC flash verify failed (FLERR)"));
            }
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("SAM4S flash controller (FRDY)"))
}

/// SAM4S rev-A erratum "Read Error after a GPNVM or Lock Bit Writing": the first flash
/// read after SGPB/CGPB/SLB/CLB can return a stale value unless a dummy read at another
/// address is interposed. One discarded word read satisfies it.
fn sam4s_post_bit_write_dummy_read<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    target.read_word(SAM4S_FLASH0_BASE + SAM4S_PAGE as u32)?;
    Ok(())
}

/// EEFC0 user-interface base: the controller for the plane mapped at [`SAM3X_FLASH0_BASE`].
pub const SAM3X_EEFC0: u32 = 0x400e_0a00;
/// EEFC1 user-interface base: the controller for the plane mapped at [`SAM3X_FLASH1_BASE`].
pub const SAM3X_EEFC1: u32 = 0x400e_0c00;
/// Plane-0 flash window (EEFC0), 256 KB; also mirrored at 0x0 when GPNVM boot-from-flash is set.
pub const SAM3X_FLASH0_BASE: u32 = 0x0008_0000;
/// Plane-1 flash window (EEFC1), 256 KB, immediately after plane 0.
pub const SAM3X_FLASH1_BASE: u32 = 0x000c_0000;
/// SAM3X flash page size in bytes (the EEFC write granularity).
pub const SAM3X_PAGE: usize = 256;
/// Each plane's size in bytes.
pub const SAM3X_PLANE_SIZE: u32 = 256 * 1024;
/// GPNVM bit index of the security bit (set = the debug port is locked out).
pub const SAM3X_GPNVM_SECURITY: u32 = 0;
/// GPNVM bit index of the boot-mode bit (set = boot flash, clear = boot the SAM-BA ROM).
pub const SAM3X_GPNVM_BOOT_FLASH: u32 = 1;
/// GPNVM bit index of the flash plane-swap bit (set = plane 1 is mapped at 0x00080000, swapping the
/// two 256 KB planes). [`SAM3X_FLASH0_BASE`] / [`SAM3X_FLASH1_BASE`] assume it is CLEAR -- the reset
/// state a bare Due is in; set it and the two plane windows exchange addresses.
pub const SAM3X_GPNVM_PLANE_SWAP: u32 = 2;

const SAM3X_FCR: u32 = 0x04;
const SAM3X_FSR: u32 = 0x08;
const SAM3X_FRR: u32 = 0x0c;
const SAM3X_FKEY: u32 = 0x5a << 24;
const SAM3X_CMD_GETD: u32 = 0x00;
const SAM3X_CMD_EWP: u32 = 0x03;
const SAM3X_CMD_EA: u32 = 0x05;
const SAM3X_CMD_CLB: u32 = 0x09;
const SAM3X_CMD_GLB: u32 = 0x0a;
const SAM3X_CMD_SGPB: u32 = 0x0b;
const SAM3X_CMD_CGPB: u32 = 0x0c;
const SAM3X_CMD_GGPB: u32 = 0x0d;
const SAM3X_FSR_FRDY: u32 = 1 << 0;
const SAM3X_FSR_FCMDE: u32 = 1 << 1;
const SAM3X_FSR_FLOCKE: u32 = 1 << 2;

/// The first words of the flash descriptor a GETD command returns -- the live geometry cross-check.
/// On a SAM3X8E each controller reports `size` = 256 KB, `page_size` = 256, `planes` = 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sam3xFlashDescriptor {
    /// FL_ID: flash interface description.
    pub interface: u32,
    /// FL_SIZE: this controller's plane size in bytes.
    pub size: u32,
    /// FL_PAGE_SIZE: page size in bytes (256 on the SAM3X).
    pub page_size: u32,
    /// FL_NB_PLANE: number of planes this controller fronts.
    pub planes: u32,
}

/// SAM3X / SAM3A (ATSAM3X8E, ...) flash programming, added to a CMSIS-DAP [`TargetAccess`] probe. `eefc`
/// selects the controller ([`SAM3X_EEFC0`] / [`SAM3X_EEFC1`]); page numbers are relative to that
/// controller's plane, whose flash window is `plane_base` ([`SAM3X_FLASH0_BASE`] /
/// [`SAM3X_FLASH1_BASE`]). Halt the core before erasing or writing so it is not fetching from flash.
pub trait Sam3xFlash {
    /// The controller's flash descriptor (GETD): the live geometry cross-check.
    fn sam3x_flash_descriptor(&mut self, eefc: u32) -> Result<Sam3xFlashDescriptor, ProbeError>;
    /// Programs `words` into consecutive pages starting at `first_page`, via ERASE-AND-WRITE-PAGE:
    /// each page's latch buffer is filled through the plane's flash window and then EWP erases and
    /// writes that page, so no separate erase pass is needed. A partial final page is padded with
    /// the erased value (`0xFFFF_FFFF`) to fill the latch buffer, per the ascending-full-buffer
    /// procedure.
    fn sam3x_write_flash(
        &mut self,
        eefc: u32,
        plane_base: u32,
        first_page: u32,
        words: &[u32],
    ) -> Result<(), ProbeError>;
    /// Erases the ENTIRE plane fronted by `eefc` (EA) -- the only bulk erase the SAM3X offers below
    /// whole-plane granularity is the per-page erase folded into [`sam3x_write_flash`](Self::sam3x_write_flash).
    fn sam3x_erase_all(&mut self, eefc: u32) -> Result<(), ProbeError>;
    /// The plane's lock bits (bit n = lock region n of 16 KB / 64 pages), via GLB. 16 regions per
    /// plane, so the low 16 bits are meaningful.
    fn sam3x_lock_bits(&mut self, eefc: u32) -> Result<u32, ProbeError>;
    /// Clears the lock bit of the region containing `page`, via CLB.
    fn sam3x_clear_lock(&mut self, eefc: u32, page: u32) -> Result<(), ProbeError>;
    /// The GPNVM bits (bit 0 = security, bit 1 = boot mode), via EEFC0 GGPB.
    fn sam3x_gpnvm_bits(&mut self) -> Result<u32, ProbeError>;
    /// Sets GPNVM bit `bit`, via EEFC0 SGPB.
    fn sam3x_set_gpnvm(&mut self, bit: u32) -> Result<(), ProbeError>;
    /// Clears GPNVM bit `bit`, via EEFC0 CGPB.
    fn sam3x_clear_gpnvm(&mut self, bit: u32) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Sam3xFlash for A {
    fn sam3x_flash_descriptor(&mut self, eefc: u32) -> Result<Sam3xFlashDescriptor, ProbeError> {
        sam3x_command(self, eefc, SAM3X_CMD_GETD, 0)?;
        Ok(Sam3xFlashDescriptor {
            interface: self.read_word(eefc + SAM3X_FRR)?,
            size: self.read_word(eefc + SAM3X_FRR)?,
            page_size: self.read_word(eefc + SAM3X_FRR)?,
            planes: self.read_word(eefc + SAM3X_FRR)?,
        })
    }

    fn sam3x_write_flash(
        &mut self,
        eefc: u32,
        plane_base: u32,
        first_page: u32,
        words: &[u32],
    ) -> Result<(), ProbeError> {
        const PAGE_WORDS: usize = SAM3X_PAGE / 4;
        for (index, chunk) in words.chunks(PAGE_WORDS).enumerate() {
            let page = first_page + index as u32;
            let page_addr = plane_base + page * SAM3X_PAGE as u32;
            if chunk.len() == PAGE_WORDS {
                self.write_words(page_addr, chunk)?;
            } else {
                let mut full = [0xffff_ffffu32; PAGE_WORDS];
                full[..chunk.len()].copy_from_slice(chunk);
                self.write_words(page_addr, &full)?;
            }
            sam3x_command(self, eefc, SAM3X_CMD_EWP, page)?;
        }
        Ok(())
    }

    fn sam3x_erase_all(&mut self, eefc: u32) -> Result<(), ProbeError> {
        sam3x_command(self, eefc, SAM3X_CMD_EA, 0)
    }

    fn sam3x_lock_bits(&mut self, eefc: u32) -> Result<u32, ProbeError> {
        sam3x_command(self, eefc, SAM3X_CMD_GLB, 0)?;
        self.read_word(eefc + SAM3X_FRR)
    }

    fn sam3x_clear_lock(&mut self, eefc: u32, page: u32) -> Result<(), ProbeError> {
        sam3x_command(self, eefc, SAM3X_CMD_CLB, page)
    }

    fn sam3x_gpnvm_bits(&mut self) -> Result<u32, ProbeError> {
        sam3x_command(self, SAM3X_EEFC0, SAM3X_CMD_GGPB, 0)?;
        self.read_word(SAM3X_EEFC0 + SAM3X_FRR)
    }

    fn sam3x_set_gpnvm(&mut self, bit: u32) -> Result<(), ProbeError> {
        sam3x_command(self, SAM3X_EEFC0, SAM3X_CMD_SGPB, bit)
    }

    fn sam3x_clear_gpnvm(&mut self, bit: u32) -> Result<(), ProbeError> {
        sam3x_command(self, SAM3X_EEFC0, SAM3X_CMD_CGPB, bit)
    }
}

/// Issues one EEFC command (key | arg | cmd into FCR) and waits for FRDY, mapping the FSR error
/// flags: FCMDE = bad key/command, FLOCKE = the command hit a locked region and was refused. (The
/// SAM3X FSR defines no FLERR verify-error bit.)
fn sam3x_command<A: TargetAccess>(
    target: &mut A,
    eefc: u32,
    cmd: u32,
    arg: u32,
) -> Result<(), ProbeError> {
    target.write_word(eefc + SAM3X_FCR, SAM3X_FKEY | (arg << 8) | cmd)?;
    for _ in 0..4000 {
        let fsr = target.read_word(eefc + SAM3X_FSR)?;
        if fsr & SAM3X_FSR_FRDY != 0 {
            if fsr & SAM3X_FSR_FCMDE != 0 {
                return Err(ProbeError::Device("SAM3X EEFC command error (FCMDE)"));
            }
            if fsr & SAM3X_FSR_FLOCKE != 0 {
                return Err(ProbeError::Device("SAM3X EEFC lock violation (FLOCKE)"));
            }
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("SAM3X flash controller (FRDY)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cmsis_dap::{Dap, proto};
    use lamella_probe_core::ArmDap;
    use lamella_cmsis_dap::testing::{Mock, echo};

    #[test]
    fn erase_row_drives_nvmctrl() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let ready = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready,
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.erase_flash_row(0x0000_0100).unwrap();
        assert_eq!(&target.inner().transport().sent[1][4..8], &0x80u32.to_le_bytes());
        assert_eq!(
            &target.inner().transport().sent[3][4..8],
            &0x0000_a502u32.to_le_bytes()
        );
    }

    #[test]
    fn write_flash_fills_buffer_then_writes_page() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let ready = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00];
        let ctrlb = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00];
        let flash = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0xff, 0xff, 0xff, 0xff];
        let replies = vec![
            ack.clone(),
            ctrlb,
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready.clone(),
            ack.clone(),
            block_ack(1),
            ack.clone(),
            flash,
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready,
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        Samd21Flash::write_flash(&mut target, 0x0, &[0xcafe_babe]).unwrap();
        assert_eq!(&target.inner().transport().sent[3][4..8], &0x80u32.to_le_bytes());
        assert_eq!(target.inner().transport().sent[9][0], proto::cmd::TRANSFER_BLOCK);
        assert_eq!(&target.inner().transport().sent[9][5..9], &0xcafe_babeu32.to_le_bytes());
        assert_eq!(
            &target.inner().transport().sent[15][4..8],
            &0x0000_a504u32.to_le_bytes()
        );
    }

    /// STATUS.READY set, seen through the 32-bit read at INTFLAG (+0x10): bit 16.
    fn same54_ready_reply() -> Vec<u8> {
        vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00]
    }

    #[test]
    fn same54_erase_block_drives_ctrlb() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let replies = vec![
            ack.clone(),
            same54_ready_reply(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            same54_ready_reply(),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.erase_flash_block(0x0000_2345).unwrap();
        assert_eq!(&target.inner().transport().sent[3][4..8], &0x2000u32.to_le_bytes());
        assert_eq!(
            &target.inner().transport().sent[5][4..8],
            &0x0000_a501u32.to_le_bytes()
        );
    }

    #[test]
    fn same54_write_flash_clears_wmode_then_writes_page() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let ctrla = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x14, 0x00, 0x00, 0x00];
        let replies = vec![
            ack.clone(),
            ctrla,
            ack.clone(),
            ack.clone(),
            ack.clone(),
            same54_ready_reply(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            same54_ready_reply(),
            ack.clone(),
            block_ack(1),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            same54_ready_reply(),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        Same54Flash::write_flash(&mut target, 0x0, &[0xcafe_babe]).unwrap();
        assert_eq!(&target.inner().transport().sent[3][4..8], &0x04u32.to_le_bytes());
        assert_eq!(
            &target.inner().transport().sent[7][4..8],
            &0x0000_a515u32.to_le_bytes()
        );
        assert_eq!(target.inner().transport().sent[11][0], proto::cmd::TRANSFER_BLOCK);
        assert_eq!(&target.inner().transport().sent[11][5..9], &0xcafe_babeu32.to_le_bytes());
        assert_eq!(&target.inner().transport().sent[13][4..8], &0x0u32.to_le_bytes());
        assert_eq!(
            &target.inner().transport().sent[15][4..8],
            &0x0000_a503u32.to_le_bytes()
        );
    }

    /// EEFC FSR read reply: FRDY set, no error flags.
    fn sam4s_ready_reply() -> Vec<u8> {
        vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00]
    }

    /// EEFC FSR read reply: FRDY clear (controller busy / STUI window open).
    fn sam4s_busy_reply() -> Vec<u8> {
        vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00]
    }

    /// A `DAP_TransferBlock` write acknowledge for `count` completed transfers.
    fn block_ack(count: u16) -> Vec<u8> {
        let c = count.to_le_bytes();
        vec![proto::cmd::TRANSFER_BLOCK, c[0], c[1], 0x01]
    }

    #[test]
    fn sam4s_erase_pages8_encodes_epa() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            sam4s_ready_reply(),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.sam4s_erase_pages8(SAM4S_EEFC0, 16).unwrap();
        assert_eq!(&target.inner().transport().sent[0][4..8], &0x400e_0a04u32.to_le_bytes());
        assert_eq!(&target.inner().transport().sent[1][4..8], &0x5a00_1107u32.to_le_bytes());
    }

    #[test]
    fn sam4s_write_flash_fills_latch_ascending_then_wp() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let mut replies = vec![ack.clone()];
        for _ in 0..9 {
            replies.push(block_ack(14));
        }
        replies.push(block_ack(2));
        replies.push(ack.clone());
        replies.push(ack.clone());
        replies.push(ack.clone());
        replies.push(sam4s_ready_reply());
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.sam4s_write_flash(SAM4S_EEFC0, SAM4S_FLASH0_BASE, 3, &[0xcafe_babe]).unwrap();
        let sent = &target.inner().transport().sent;
        assert_eq!(&sent[0][4..8], &(0x0040_0000u32 + 3 * 512).to_le_bytes());
        assert_eq!(sent[1][0], proto::cmd::TRANSFER_BLOCK);
        assert_eq!(&sent[1][5..9], &0xcafe_babeu32.to_le_bytes());
        assert_eq!(&sent[1][9..13], &0xffff_ffffu32.to_le_bytes());
        assert!(sent[2..=10].iter().all(|s| s[0] == proto::cmd::TRANSFER_BLOCK));
        assert_eq!(&sent[12][4..8], &0x5a00_0301u32.to_le_bytes());
    }

    #[test]
    fn sam4s_set_gpnvm_drives_eefc0_then_dummy_reads() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let flash_word = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0xee, 0xff, 0xc0, 0x00];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            sam4s_ready_reply(),
            ack.clone(),
            flash_word,
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.sam4s_set_gpnvm(SAM4S_GPNVM_BOOT_FLASH).unwrap();
        let sent = &target.inner().transport().sent;
        assert_eq!(&sent[0][4..8], &0x400e_0a04u32.to_le_bytes());
        assert_eq!(&sent[1][4..8], &0x5a00_010bu32.to_le_bytes());
        assert_eq!(&sent[4][4..8], &(0x0040_0000u32 + 512).to_le_bytes());
    }

    #[test]
    fn sam4s_unique_id_reads_plane_window_between_stui_and_spui() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let id_word = |b: u8| vec![proto::cmd::TRANSFER, 0x01, 0x01, b, 0x00, 0x00, 0x00];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            sam4s_busy_reply(),
            ack.clone(),
            id_word(0x11),
            ack.clone(),
            id_word(0x22),
            ack.clone(),
            id_word(0x33),
            ack.clone(),
            id_word(0x44),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            sam4s_ready_reply(),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let id = target.sam4s_unique_id(SAM4S_EEFC0, SAM4S_FLASH0_BASE).unwrap();
        assert_eq!(id, [0x11, 0x22, 0x33, 0x44]);
        let sent = &target.inner().transport().sent;
        assert_eq!(&sent[1][4..8], &0x5a00_000eu32.to_le_bytes());
        assert_eq!(&sent[4][4..8], &0x0040_0000u32.to_le_bytes());
        assert_eq!(&sent[13][4..8], &0x5a00_000fu32.to_le_bytes());
    }

    /// An EEFC FRR result-word read reply carrying `v`.
    fn frr_word(v: u32) -> Vec<u8> {
        let b = v.to_le_bytes();
        vec![proto::cmd::TRANSFER, 0x01, 0x01, b[0], b[1], b[2], b[3]]
    }

    #[test]
    fn sam3x_write_flash_fills_latch_ascending_then_ewp() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let mut replies = vec![ack.clone()];
        for &count in &[14u16, 14, 14, 14, 8] {
            replies.push(block_ack(count));
        }
        replies.push(ack.clone());
        replies.push(ack.clone());
        replies.push(ack.clone());
        replies.push(sam4s_ready_reply());
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.sam3x_write_flash(SAM3X_EEFC0, SAM3X_FLASH0_BASE, 3, &[0xcafe_babe]).unwrap();
        let sent = &target.inner().transport().sent;
        assert_eq!(&sent[0][4..8], &(SAM3X_FLASH0_BASE + 3 * 256).to_le_bytes());
        assert_eq!(sent[1][0], proto::cmd::TRANSFER_BLOCK);
        assert_eq!(&sent[1][5..9], &0xcafe_babeu32.to_le_bytes());
        assert_eq!(&sent[1][9..13], &0xffff_ffffu32.to_le_bytes());
        assert!(sent[2..=5].iter().all(|s| s[0] == proto::cmd::TRANSFER_BLOCK));
        assert_eq!(&sent[7][4..8], &0x5a00_0303u32.to_le_bytes());
    }

    #[test]
    fn sam3x_erase_all_targets_the_named_controller() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let replies = vec![ack.clone(), ack.clone(), ack.clone(), sam4s_ready_reply()];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.sam3x_erase_all(SAM3X_EEFC1).unwrap();
        let sent = &target.inner().transport().sent;
        assert_eq!(&sent[0][4..8], &(SAM3X_EEFC1 + 0x04).to_le_bytes());
        assert_eq!(&sent[1][4..8], &0x5a00_0005u32.to_le_bytes());
    }

    #[test]
    fn sam3x_set_gpnvm_drives_eefc0() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let replies = vec![ack.clone(), ack.clone(), ack.clone(), sam4s_ready_reply()];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.sam3x_set_gpnvm(SAM3X_GPNVM_BOOT_FLASH).unwrap();
        let sent = &target.inner().transport().sent;
        assert_eq!(&sent[0][4..8], &0x400e_0a04u32.to_le_bytes());
        assert_eq!(&sent[1][4..8], &0x5a00_010bu32.to_le_bytes());
    }

    #[test]
    fn sam3x_flash_descriptor_reads_four_frr_words() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            sam4s_ready_reply(),
            ack.clone(),
            frr_word(0x000f_0640),
            ack.clone(),
            frr_word(262_144),
            ack.clone(),
            frr_word(256),
            ack.clone(),
            frr_word(1),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let descriptor = target.sam3x_flash_descriptor(SAM3X_EEFC0).unwrap();
        assert_eq!(
            descriptor,
            Sam3xFlashDescriptor { interface: 0x000f_0640, size: 262_144, page_size: 256, planes: 1 }
        );
        assert_eq!(&target.inner().transport().sent[1][4..8], &0x5a00_0000u32.to_le_bytes());
    }

    /// A MEM-AP read reply carrying `value`.
    fn word_reply(value: u32) -> Vec<u8> {
        let b = value.to_le_bytes();
        vec![proto::cmd::TRANSFER, 0x01, 0x01, b[0], b[1], b[2], b[3]]
    }

    #[test]
    fn device_id_decodes_the_samd11_datasheet_rows() {
        for (did, part) in [
            (0x1003_0200u32, "ATSAMD11D14AM (24-pin QFN)"),
            (0x1003_0203, "ATSAMD11D14ASS (20-pin SOIC)"),
            (0x1003_0206, "ATSAMD11C14A (14-pin SOIC)"),
            (0x1003_0209, "ATSAMD11D14AU (20-ball WLCSP)"),
        ] {
            let id = SamDeviceId::decode(did);
            assert_eq!(id.part(), Some(part), "DID {did:#010x}");
            assert_eq!((id.processor, id.family, id.series), (0x1, 0x0, 0x3));
            assert_eq!((id.core(), id.revision_letter()), (Some("Cortex-M0+"), 'C'));
            assert!(id.drives_samd21_nvmctrl(), "the D11 NVMCTRL is the D21 routine's");
        }
    }

    #[test]
    fn device_id_decodes_the_samd10_datasheet_rows() {
        for (did, part) in [
            (0x1002_0000u32, "ATSAMD10D14AM (24-pin QFN)"),
            (0x1002_0001, "ATSAMD10D13AM (24-pin QFN)"),
            (0x1002_0007, "ATSAMD10C13A (14-pin SOIC)"),
            (0x1002_0009, "ATSAMD10D14AU (20-ball WLCSP)"),
        ] {
            let id = SamDeviceId::decode(did);
            assert_eq!(id.part(), Some(part), "DID {did:#010x}");
            assert_eq!((id.processor, id.family, id.series), (0x1, 0x0, 0x2));
            assert!(id.drives_samd21_nvmctrl(), "the D10 NVMCTRL is the D21 routine's");
        }
    }

    #[test]
    fn geometry_from_param_reports_the_samd10_eight_kilobyte_part() {
        let geometry = SamFlashGeometry::decode(0x0003_0080);
        assert_eq!((geometry.pages, geometry.page_bytes), (128, 64));
        assert_eq!(geometry.flash_bytes(), 8 * 1024);
    }

    #[test]
    fn device_id_names_the_three_parts_read_off_this_bench() {
        for (did, part, series, rev) in [
            (0x1001_0300u32, "ATSAMD21J18A", 0x1u8, 'D'),
            (0x1003_0100, "ATSAMD11D14AM (24-pin QFN)", 0x3, 'B'),
            (0x1002_0100, "ATSAMD10D14AM (24-pin QFN)", 0x2, 'B'),
        ] {
            let id = SamDeviceId::decode(did);
            assert_eq!(id.part(), Some(part), "DID {did:#010x}");
            assert_eq!((id.series, id.die, id.devsel), (series, 0x0, 0x00));
            assert_eq!(id.revision_letter(), rev);
            assert!(id.drives_samd21_nvmctrl());
        }
    }

    #[test]
    fn device_id_leaves_an_unsourced_row_unnamed_but_still_decoded() {
        let id = SamDeviceId::decode(0x1001_0305);
        assert_eq!(id.part(), None);
        assert_eq!((id.series, id.devsel, id.core()), (0x1, 0x05, Some("Cortex-M0+")));
        assert!(id.drives_samd21_nvmctrl());
    }

    #[test]
    fn device_id_refuses_the_families_this_flash_routine_does_not_drive() {
        let e54 = SamDeviceId::decode(0x6184_0300);
        assert_eq!((e54.processor, e54.family, e54.series), (0x6, 0x3, 0x4));
        assert!(!e54.drives_samd21_nvmctrl());
        assert!(e54.drives_same54_nvmctrl());
        assert_eq!(e54.flash_routine(), Some("Same54Flash"));
    }

    /// The two routines must claim DISJOINT parts, and a part neither claims must report neither.
    ///
    /// The defect this pins is not a wrong predicate -- both were right -- but a tool that read one
    /// predicate's `false` as "this crate cannot flash this part", when the other routine drives it.
    #[test]
    fn each_part_is_claimed_by_at_most_one_flash_routine() {
        for raw in [0x1001_0000u32, 0x1003_0100, 0x1002_0000] {
            let id = SamDeviceId::decode(raw);
            assert!(id.drives_samd21_nvmctrl(), "{raw:#010x} is a D21-class NVMCTRL");
            assert!(!id.drives_same54_nvmctrl());
            assert_eq!(id.flash_routine(), Some("Samd21Flash"));
        }
        let e54 = SamDeviceId::decode(0x6184_0300);
        assert!(e54.drives_same54_nvmctrl() && !e54.drives_samd21_nvmctrl());

        let unread = SamDeviceId::decode(0x6018_0400);
        assert!(!unread.drives_samd21_nvmctrl() && !unread.drives_same54_nvmctrl());
        assert_eq!(unread.flash_routine(), None);
    }

    #[test]
    fn geometry_from_param_agrees_with_the_samd21_flash_constants() {
        let geometry = SamFlashGeometry::decode(0x0003_1000);
        assert_eq!(geometry.page_bytes, SAMD21_PAGE as u32);
        assert_eq!(geometry.samd21_row_bytes(), SAMD21_ROW);
        assert_eq!(geometry.flash_bytes(), 256 * 1024);
    }

    #[test]
    fn geometry_from_param_reports_the_samd11_sixteen_kilobytes() {
        let geometry = SamFlashGeometry::decode(0x0003_0100);
        assert_eq!((geometry.pages, geometry.page_bytes), (256, 64));
        assert_eq!(geometry.flash_bytes(), 16 * 1024);
        assert_eq!(geometry.samd21_row_bytes(), 256);
    }

    #[test]
    fn sam_device_id_reads_the_dsu_register() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let replies = vec![ack, word_reply(0x1003_0000)];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let id = target.sam_device_id().unwrap();
        assert_eq!(&target.inner().transport().sent[0][4..8], &SAM_DSU_DID.to_le_bytes());
        assert_eq!(id.part(), Some("ATSAMD11D14AM (24-pin QFN)"));
    }

    #[test]
    fn sam_flash_geometry_reads_nvmctrl_param() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let replies = vec![ack, word_reply(0x0003_0100)];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let geometry = target.sam_flash_geometry().unwrap();
        assert_eq!(&target.inner().transport().sent[0][4..8], &SAM_NVMCTRL_PARAM.to_le_bytes());
        assert_eq!(geometry.flash_bytes(), 16 * 1024);
    }

    #[test]
    fn a_zero_internal_dsu_view_is_retried_at_the_external_one() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let replies =
            vec![ack.clone(), word_reply(0), ack, word_reply(0x1001_0300)];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let id = target.sam_device_id().unwrap();
        assert_eq!(id.part(), Some("ATSAMD21J18A"));
        assert_eq!(&target.inner().transport().sent[0][4..8], &SAM_DSU_DID.to_le_bytes());
        assert_eq!(
            &target.inner().transport().sent[2][4..8],
            &SAM_DSU_DID_EXTERNAL.to_le_bytes()
        );
    }

    #[test]
    fn a_dead_read_path_is_refused_rather_than_decoded() {
        for dead in [0x0000_0000u32, 0xffff_ffff] {
            let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
            let replies =
                vec![ack.clone(), word_reply(dead), ack, word_reply(dead)];
            let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
            assert!(target.sam_device_id().is_err(), "DID {dead:#010x} decoded instead of refusing");
        }
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let mut target = ArmDap::new(Dap::new(Mock::new(vec![ack, word_reply(0)])));
        assert!(target.sam_flash_geometry().is_err(), "a zero-page PARAM decoded instead of refusing");
    }
}
