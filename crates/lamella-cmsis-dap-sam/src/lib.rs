//! Microchip SAM (Atmel SAM) flash programming over a Lamella debug probe.

use lamella_probe_core::{ProbeError, TargetAccess};

const SAMD21_CTRLA: u32 = 0x4100_4000;
const SAMD21_CTRLB: u32 = 0x4100_4004;
const SAMD21_INTFLAG: u32 = 0x4100_4014;
const SAMD21_ADDR: u32 = 0x4100_401c;
const SAMD21_LOCK: u32 = 0x4100_4020;
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
            let _ = self.reset_extension();
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
        let _ = self.reset_extension();
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

/// SAM D21 flash programming, added to a CMSIS-DAP [`TargetAccess`] probe. Halt the core before
/// erasing or writing so it is not fetching from flash during the operation.
///
/// # It is the SAM D10's and SAM D11's sequence too, and that is a read rather than an assumption
///
/// The three parts agree on every fact this routine uses, and each was looked up in its OWN
/// datasheet rather than carried across:
///
/// ```text
/// NVMCTRL base   0x41004000   all three
/// write unit     64 bytes     a PAGE; PARAM.PSZ encodes 8..1024 and not every family offers all
/// erase unit     a ROW of four pages, and erasing sets every bit to one
/// ```
///
/// **THE AGREEMENT IS NOT DERIVABLE FROM ONE OF THEM.** `csp/samd21/blocks/flash.toml` says so in
/// its own words -- the 64-byte page "is what makes it a block fact HERE and would not make it one
/// for a different SAM family" -- because the page size is a field the controller reports and the
/// datasheets state plainly that device families differ. So three documents were read, and the
/// answer being the same three times is the finding.
///
/// It is reused under this name rather than copied under three, the same choice
/// [`Stm32F4Flash`](../../lamella_cmsis_dap_stm32/trait.Stm32F4Flash.html) makes for the STM32F7:
/// one sequence with a stated scope beats three files that drift.
///
/// **ALL THREE PATHS ARE EXERCISED ON SILICON.** The D21 has been all along; the D10 and D11 were
/// settled on an ATSAMD10D14AM and an ATSAMD11D14AM, each erased whole and programmed through THIS
/// trait's `erase_flash_row` and `write_flash` -- `samd-erase-all` and `deploy-samd-raw` are the
/// tools -- and each then left running an image whose counter was read advancing. So a caller
/// reaching for this on a D10 or a D11 is relying on a part that accepted the sequence, not only
/// on three datasheets agreeing.
pub trait Samd21Flash {
    /// Erases the flash row (256 bytes) containing `address`, via the NVMCTRL.
    fn erase_flash_row(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs consecutive 32-bit `words` to flash from `address`, via the NVMCTRL, one 64-byte
    /// page at a time (the rows must already be erased).
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError>;
    /// Reads NVMCTRL.LOCK, the region locks in force now. Bit n is region n's lock, and a region
    /// whose bit reads 0 is locked (DS40001882D 22.8).
    ///
    /// The part loads the register from the user row's LOCK field at every reset, and a Lock or
    /// Unlock Region command changes it until the next one (22.6.3). A part whose regions are all
    /// unlocked reads 0xFFFF, as Table 10-4's default for that field says; 22.8's own line that
    /// the state after an erase is "unlocked (0x0000)" contradicts both.
    fn read_samd21_region_locks(&mut self) -> Result<u16, ProbeError>;
}

impl<A: TargetAccess> Samd21Flash for A {
    fn read_samd21_region_locks(&mut self) -> Result<u16, ProbeError> {
        Ok((self.read_word(SAMD21_LOCK)? & 0xffff) as u16)
    }

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


/// Where the SAM D21 maps its NVM User Row: the start of the auxiliary space's AUX0 area
/// (DS40001882D 10.3.1).
pub const SAMD21_USER_ROW: u32 = 0x0080_4000;

/// The NVM User Row's length in 32-bit words: one row, which is the controller's erase unit.
pub const SAMD21_USER_ROW_WORDS: usize = SAMD21_ROW as usize / 4;

/// BOOTPROT's value when it protects no rows (DS40001882D Table 22-2). It is also the factory
/// value, except on the WLCSP package.
pub const SAMD21_BOOTPROT_NONE: u8 = 7;

/// The EEPROM field's value when it reserves no rows (DS40001882D Table 22-3).
pub const SAMD21_EEPROM_NONE: u8 = 7;

/// How many equally sized lock regions the main array is grouped into (DS40001882D 22.6.3): a
/// region is the array's size over this, 16 KB on a 256 KB part.
pub const SAMD21_LOCK_REGIONS: u32 = 16;

/// Where the SAM D21 keeps the four words of its 128-bit serial number, in order (DS40001882D
/// 10.3.3). Only all four together are unique to one part.
pub const SAMD21_SERIAL_NUMBER: [u32; 4] = [0x0080_a00c, 0x0080_a040, 0x0080_a044, 0x0080_a048];

const SAMD21_STATUS: u32 = 0x4100_4018;
const SAMD21_STATUS_PROGE: u32 = 1 << 2;
const SAMD21_STATUS_LOCKE: u32 = 1 << 3;
const SAMD21_STATUS_NVME: u32 = 1 << 4;
const SAMD21_STATUS_ERRORS: u32 = SAMD21_STATUS_PROGE | SAMD21_STATUS_LOCKE | SAMD21_STATUS_NVME;
const SAMD21_STATUS_SB: u32 = 1 << 8;
const SAMD21_CMD_EAR: u32 = 0x05;
const SAMD21_CMD_WAP: u32 = 0x06;
const SAMD21_BOOTPROT_MASK: u32 = 0b111;
const SAMD21_EEPROM_SHIFT: u32 = 4;
const SAMD21_EEPROM_MASK: u32 = 0b111;
const SAMD21_LOCK_SHIFT: u32 = 16;
const SAMD21_USER_ROW_ATTEMPTS: usize = 3;

/// The SAM D21's NVM User Row, as read from the part.
///
/// Its first two words hold the part's power-on configuration (DS40001882D Table 10-4); the rest
/// of the row holds no defined field. The configuration takes effect at the part's next reset,
/// not when the row is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Samd21UserRow {
    /// The row's words in address order, from [`SAMD21_USER_ROW`].
    pub words: [u32; SAMD21_USER_ROW_WORDS],
}

/// A field of the NVM User Row's configuration, as DS40001882D Table 10-4 lays it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Samd21UserRowField {
    /// The field's name.
    pub name: &'static str,
    /// The field's lowest bit, counted across the row's first two words: bit 0 of the second word
    /// is bit 32.
    pub low_bit: u32,
    /// The field's width in bits.
    pub width: u32,
}

/// Every field in the first 64 bits of the NVM User Row, reserved ones included, in bit order
/// (DS40001882D Table 10-4). Together they cover each bit exactly once.
///
/// The two fields named `BOD12 configuration` are written in production and must not be changed.
pub const SAMD21_USER_ROW_FIELDS: &[Samd21UserRowField] = &[
    user_row_field("BOOTPROT", 0, 3),
    user_row_field("reserved", 3, 1),
    user_row_field("EEPROM", 4, 3),
    user_row_field("reserved", 7, 1),
    user_row_field("BOD33 level", 8, 6),
    user_row_field("BOD33 enable", 14, 1),
    user_row_field("BOD33 action", 15, 2),
    user_row_field("BOD12 configuration", 17, 8),
    user_row_field("WDT enable", 25, 1),
    user_row_field("WDT always-on", 26, 1),
    user_row_field("WDT period", 27, 4),
    user_row_field("WDT window", 31, 4),
    user_row_field("WDT early-warning offset", 35, 4),
    user_row_field("WDT window mode", 39, 1),
    user_row_field("BOD33 hysteresis", 40, 1),
    user_row_field("BOD12 configuration", 41, 1),
    user_row_field("reserved", 42, 6),
    user_row_field("LOCK", 48, 16),
];

const fn user_row_field(name: &'static str, low_bit: u32, width: u32) -> Samd21UserRowField {
    Samd21UserRowField { name, low_bit, width }
}

impl Samd21UserRow {
    /// A row from the bytes a backup holds, in address order. `None` unless there are exactly 256.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != SAMD21_USER_ROW_WORDS * 4 {
            return None;
        }
        let mut words = [0u32; SAMD21_USER_ROW_WORDS];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(4)) {
            *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        Some(Samd21UserRow { words })
    }

    /// The row's bytes in address order, which is what a backup of it holds.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    /// The value `field` holds in this row.
    pub fn field(&self, field: &Samd21UserRowField) -> u32 {
        let configuration = u64::from(self.words[0]) | (u64::from(self.words[1]) << 32);
        ((configuration >> field.low_bit) & ((1u64 << field.width) - 1)) as u32
    }

    /// BOOTPROT: which bootloader size the part protects from address zero.
    pub fn bootprot(&self) -> u8 {
        (self.words[0] & SAMD21_BOOTPROT_MASK) as u8
    }

    /// EEPROM: how many rows at the top of the main array the part reserves as EEPROM.
    pub fn eeprom(&self) -> u8 {
        ((self.words[0] >> SAMD21_EEPROM_SHIFT) & SAMD21_EEPROM_MASK) as u8
    }

    /// LOCK: the region locks the part loads into NVMCTRL.LOCK at each reset, one bit per region,
    /// where 0 locks the region.
    pub fn lock(&self) -> u16 {
        (self.words[1] >> SAMD21_LOCK_SHIFT) as u16
    }

    /// The same row with BOOTPROT set to `bootprot`, and every other bit as it was.
    ///
    /// `None` when `bootprot` does not fit the field's three bits.
    pub fn with_bootprot(&self, bootprot: u8) -> Option<Self> {
        if u32::from(bootprot) > SAMD21_BOOTPROT_MASK {
            return None;
        }
        let mut words = self.words;
        words[0] = (words[0] & !SAMD21_BOOTPROT_MASK) | u32::from(bootprot);
        Some(Samd21UserRow { words })
    }

    /// Whether a rewrite of `saved` that kept every bit but BOOTPROT could have left this row
    /// part-way: every bit `saved` holds at one, outside BOOTPROT, still reads one here.
    ///
    /// Programming only turns ones into zeros. So an erased row is part-way to any row, a row that
    /// differs from `saved` in BOOTPROT alone is part-way to it, and a row with a bit at zero that
    /// `saved` holds at one did not come from `saved` by such a rewrite.
    pub fn is_partway_to(&self, saved: &Samd21UserRow) -> bool {
        self.words
            .iter()
            .zip(saved.words.iter())
            .enumerate()
            .all(|(index, (now, saved))| {
                let kept = if index == 0 { !SAMD21_BOOTPROT_MASK } else { u32::MAX };
                now & saved & kept == saved & kept
            })
    }
}

/// How many bytes a BOOTPROT value protects from address zero (DS40001882D Table 22-2): none at 7,
/// then 512 bytes, doubling to 32 KB as the value falls to 0.
pub fn samd21_bootprot_bytes(bootprot: u8) -> u32 {
    match bootprot & SAMD21_BOOTPROT_MASK as u8 {
        SAMD21_BOOTPROT_NONE => 0,
        value => 32_768 >> value,
    }
}

/// How many bytes an EEPROM value reserves at the top of the main array (DS40001882D Table 22-3):
/// none at 7, then 256 bytes, doubling to 16 KB as the value falls to 0.
///
/// Those rows are written whatever their region's lock says (22.6.5).
pub fn samd21_eeprom_bytes(eeprom: u8) -> u32 {
    match eeprom & SAMD21_EEPROM_MASK as u8 {
        SAMD21_EEPROM_NONE => 0,
        value => 16_384 >> value,
    }
}

/// Why a rewrite of the SAM D21's NVM User Row stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Samd21UserRowError {
    /// Stopped before anything was erased or written. The row is as it was read.
    Unchanged(String),
    /// The row was erased, and it did not read back as intended after every attempt to program it.
    ///
    /// The part keeps running on its previous configuration until its next reset, because a
    /// written row takes effect only then (DS40001882D 10.3.1).
    NotRewritten {
        /// What stopped the last attempt.
        reason: String,
        /// The row as the last attempt read it back, or `None` when it could not be read.
        now: Option<Box<Samd21UserRow>>,
    },
}

impl std::fmt::Display for Samd21UserRowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Samd21UserRowError::Unchanged(why) => write!(f, "{why}; nothing was written"),
            Samd21UserRowError::NotRewritten { reason, .. } => write!(
                f,
                "the user row was erased and did not read back as intended: {reason}"
            ),
        }
    }
}

/// Reading and rewriting the SAM D21's NVM User Row (DS40001882D 10.3.1).
///
/// The row holds the part's power-on configuration, and two of its fields hold BOD12 settings that
/// are written in production and must not be changed. So two writes are offered and no more: one
/// changes BOOTPROT and takes every other bit from the row as it was read, and one puts a saved
/// copy of the row back, over a BOOTPROT rewrite finished or not.
pub trait Samd21UserRowAccess {
    /// Reads the whole NVM User Row. It writes nothing, and the core need not be halted.
    fn read_samd21_user_row(&mut self) -> Result<Samd21UserRow, ProbeError>;

    /// Reads the part's 128-bit serial number, word 0 first. A saved copy of a user row belongs to
    /// the part with this number, because two BOD12 fields in the row are calibrated per part.
    fn read_samd21_serial_number(&mut self) -> Result<[u32; 4], ProbeError>;

    /// Rewrites the NVM User Row with BOOTPROT set to `bootprot` and every other bit taken from
    /// `as_read`, then reads it back.
    ///
    /// `as_read` is the row the caller read and kept a copy of. Nothing is written unless the row
    /// still reads exactly as that copy, the core is halted, the security bit is clear and the
    /// flash controller is idle. When BOOTPROT already holds `bootprot`, nothing is written and
    /// the row is returned as it reads.
    ///
    /// The row is erased, then programmed a page at a time, and each page that holds data is
    /// programmed from the page buffer. An attempt that fails is repeated from the erase. The new
    /// configuration takes effect at the part's next reset.
    fn rewrite_samd21_bootprot(
        &mut self,
        as_read: &Samd21UserRow,
        bootprot: u8,
    ) -> Result<Samd21UserRow, Samd21UserRowError>;

    /// Writes `saved` back as the NVM User Row, then reads it back: the way back from a BOOTPROT
    /// rewrite, finished or not.
    ///
    /// `as_read` is the row the caller read and showed, and `saved` is a copy of this part's row
    /// from before the rewrite. Nothing is written unless the row still reads exactly as `as_read`,
    /// `as_read` is part-way to `saved` ([`Samd21UserRow::is_partway_to`]), and the core, the
    /// security bit and the flash controller are as [`Self::rewrite_samd21_bootprot`] requires.
    /// When the row already reads as `saved`, nothing is written and the row is returned.
    fn restore_samd21_user_row(
        &mut self,
        as_read: &Samd21UserRow,
        saved: &Samd21UserRow,
    ) -> Result<Samd21UserRow, Samd21UserRowError>;
}

impl<A: TargetAccess> Samd21UserRowAccess for A {
    fn read_samd21_user_row(&mut self) -> Result<Samd21UserRow, ProbeError> {
        let mut words = [0u32; SAMD21_USER_ROW_WORDS];
        self.read_words_into(SAMD21_USER_ROW, &mut words)?;
        Ok(Samd21UserRow { words })
    }

    fn read_samd21_serial_number(&mut self) -> Result<[u32; 4], ProbeError> {
        let mut serial = [0u32; 4];
        for (word, address) in serial.iter_mut().zip(SAMD21_SERIAL_NUMBER) {
            *word = self.read_word(address)?;
        }
        Ok(serial)
    }

    fn rewrite_samd21_bootprot(
        &mut self,
        as_read: &Samd21UserRow,
        bootprot: u8,
    ) -> Result<Samd21UserRow, Samd21UserRowError> {
        let intended = as_read.with_bootprot(bootprot).ok_or_else(|| {
            Samd21UserRowError::Unchanged(format!(
                "BOOTPROT is a three-bit field, and {bootprot} does not fit in it"
            ))
        })?;
        let now = samd21_ready_to_write_user_row(self, as_read)?;
        if now == intended {
            return Ok(now);
        }
        samd21_replace_user_row(self, &intended)
    }

    fn restore_samd21_user_row(
        &mut self,
        as_read: &Samd21UserRow,
        saved: &Samd21UserRow,
    ) -> Result<Samd21UserRow, Samd21UserRowError> {
        let now = samd21_ready_to_write_user_row(self, as_read)?;
        if now == *saved {
            return Ok(now);
        }
        if !now.is_partway_to(saved) {
            return Err(Samd21UserRowError::Unchanged(
                "the user row holds a bit at zero, outside BOOTPROT, that the saved copy holds at \
                 one, so it is not that copy part-way through a BOOTPROT rewrite; the copy is not \
                 this row's past"
                    .to_owned(),
            ));
        }
        samd21_replace_user_row(self, saved)
    }
}

/// The checks both writes make before anything is erased: the core is halted, the security bit is
/// clear, the flash controller is idle, and the row still reads as `as_read`, which it returns.
fn samd21_ready_to_write_user_row<A: TargetAccess>(
    target: &mut A,
    as_read: &Samd21UserRow,
) -> Result<Samd21UserRow, Samd21UserRowError> {
    let unchanged = |why: String| Samd21UserRowError::Unchanged(why);
    let halted = target
        .is_halted()
        .map_err(|why| unchanged(format!("reading whether the core is halted: {why}")))?;
    if !halted {
        return Err(unchanged(
            "the core is running, and firmware that drives the flash controller itself would \
             interleave its commands with these; halt the core first"
                .to_owned(),
        ));
    }
    let status = target
        .read_word(SAMD21_STATUS)
        .map_err(|why| unchanged(format!("reading the flash controller's status: {why}")))?;
    if status & SAMD21_STATUS_SB != 0 {
        return Err(unchanged(
            "the part's security bit is set, and its flash controller refuses to erase or write \
             the user row while it is"
                .to_owned(),
        ));
    }
    let ready = target
        .read_word(SAMD21_INTFLAG)
        .map_err(|why| unchanged(format!("reading the flash controller's flags: {why}")))?;
    if ready & 1 == 0 {
        return Err(unchanged(
            "the flash controller is still busy with an earlier command".to_owned(),
        ));
    }
    let now = target
        .read_samd21_user_row()
        .map_err(|why| unchanged(format!("reading the user row: {why}")))?;
    if now != *as_read {
        return Err(unchanged(
            "the user row no longer reads as the copy that was shown; read it again, and keep \
             that copy"
                .to_owned(),
        ));
    }
    Ok(now)
}

/// Erases the user row and programs `intended`, then reads it back, up to
/// [`SAMD21_USER_ROW_ATTEMPTS`] times. CTRLB goes back as it was found on every way out.
fn samd21_replace_user_row<A: TargetAccess>(
    target: &mut A,
    intended: &Samd21UserRow,
) -> Result<Samd21UserRow, Samd21UserRowError> {
    let ctrlb = target.read_word(SAMD21_CTRLB).map_err(|why| {
        Samd21UserRowError::Unchanged(format!("reading the flash controller's settings: {why}"))
    })?;
    if let Err(why) = target.write_word(SAMD21_CTRLB, ctrlb | SAMD21_MANW) {
        return Err(Samd21UserRowError::Unchanged(format!(
            "setting the flash controller to manual writes: {why}"
        )));
    }

    let mut outcome = Err(Samd21UserRowError::NotRewritten {
        reason: "no attempt ran".to_owned(),
        now: None,
    });
    for _ in 0..SAMD21_USER_ROW_ATTEMPTS {
        let attempt = samd21_erase_and_program_user_row(target, intended);
        let read_back = target.read_samd21_user_row();
        outcome = match (attempt, read_back) {
            (_, Ok(row)) if row == *intended => Ok(row),
            (Err(why), read_back) => Err(Samd21UserRowError::NotRewritten {
                reason: why,
                now: read_back.ok().map(Box::new),
            }),
            (Ok(()), Ok(row)) => Err(Samd21UserRowError::NotRewritten {
                reason: "every command completed, and the row reads back differently".to_owned(),
                now: Some(Box::new(row)),
            }),
            (Ok(()), Err(why)) => Err(Samd21UserRowError::NotRewritten {
                reason: format!("reading the row back: {why}"),
                now: None,
            }),
        };
        if outcome.is_ok() {
            break;
        }
    }
    let _ = target.write_word(SAMD21_CTRLB, ctrlb);
    outcome
}

/// One attempt at the rewrite: erase the user row, then program each page of `row` that holds any
/// zero bit. Every command's error bits are checked, and each is cleared before the next.
fn samd21_erase_and_program_user_row<A: TargetAccess>(
    target: &mut A,
    row: &Samd21UserRow,
) -> Result<(), String> {
    samd21_user_row_command(target, SAMD21_USER_ROW, SAMD21_CMD_EAR, "erasing the user row")?;
    for (page, words) in row.words.chunks(SAMD21_PAGE / 4).enumerate() {
        if words.iter().all(|word| *word == u32::MAX) {
            continue;
        }
        let address = SAMD21_USER_ROW + (page * SAMD21_PAGE) as u32;
        samd21_command(target, SAMD21_CMD_PBC)
            .map_err(|why| format!("clearing the page buffer: {why}"))?;
        target
            .write_words(address, words)
            .map_err(|why| format!("filling the page buffer for page {page}: {why}"))?;
        target
            .read_word(address)
            .map_err(|why| format!("reading back after the fill of page {page}: {why}"))?;
        samd21_user_row_command(target, address, SAMD21_CMD_WAP, "writing a user-row page")?;
    }
    Ok(())
}

/// Points ADDR at `address`, issues `command`, waits for it, and fails if the controller reports
/// an error for it. Errors left from before are cleared first, so any error seen is this command's.
fn samd21_user_row_command<A: TargetAccess>(
    target: &mut A,
    address: u32,
    command: u32,
    doing: &str,
) -> Result<(), String> {
    target
        .write_word(SAMD21_STATUS, SAMD21_STATUS_ERRORS)
        .map_err(|why| format!("{doing}: clearing the controller's error bits: {why}"))?;
    target
        .write_word(SAMD21_ADDR, address / 2)
        .map_err(|why| format!("{doing}: setting the address: {why}"))?;
    samd21_command(target, command).map_err(|why| format!("{doing}: {why}"))?;
    let status = target
        .read_word(SAMD21_STATUS)
        .map_err(|why| format!("{doing}: reading the controller's status: {why}"))?;
    let errors = status & SAMD21_STATUS_ERRORS;
    if errors != 0 {
        return Err(format!(
            "{doing}: the controller reported {}",
            samd21_status_errors(errors)
        ));
    }
    Ok(())
}

/// The error bits of a STATUS value, named for a reader.
fn samd21_status_errors(errors: u32) -> String {
    let names: Vec<&str> = [
        (SAMD21_STATUS_PROGE, "a programming error (PROGE)"),
        (SAMD21_STATUS_LOCKE, "a lock error (LOCKE)"),
        (SAMD21_STATUS_NVME, "an NVM error (NVME)"),
    ]
    .iter()
    .filter(|(bit, _)| errors & bit != 0)
    .map(|(_, name)| *name)
    .collect();
    names.join(" and ")
}

/// SAM DSU `DID` -- the part's own identification word, at DSU base + 0x18.
/// Atmel's USB vendor id, which every EDBG on an Xplained board reports.
///
/// **THE PRODUCT ID IS A BOARD FACT AND IS NOT HERE.** Xplained kits share ids by KIT rather than by
/// part -- two SAM D21 kits answer `0x2169` and three Xplained Pro kits answer `0x2111` -- so a
/// vendor/product pair narrows to a kit family and never to a board, exactly as an ST-LINK's product
/// id narrows to a probe generation. The serial rung is what settles which board, and a route that
/// skipped it would write whichever EDBG the OS handed over.
pub const EDBG_VENDOR_ID: u16 = 0x03eb;

/// Where the NVMCTRL families map their main array: address zero, which is also where they boot.
///
/// The EEFC and FLASHCALW parts do NOT -- a SAM4S maps flash at `0x00400000` and a SAM4L at
/// `0x00000000` through a different controller -- so this is named for the families it describes
/// rather than as a SAM-wide fact.
pub const SAM_NVMCTRL_FLASH_BASE: u32 = 0x0000_0000;

/// The `DID` register in the DSU's INTERNAL address range -- the die identity a D/E/L/C part
/// reports about itself (SAM D21 DS40001882, 14.12.7).
///
/// It is `pub` because a flash route reads it to refuse a board that is not the one it was built
/// for, and that check has to happen outside this crate. Its external-range twin below is the one
/// to use on a part whose security bit is set.
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
    /// that `DEVSEL`, while the fields above still give the family, series, die and revision. The
    /// alternative -- transcribing a vendor tool's table -- puts numbers here that no document
    /// sources and no part confirms.
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
            0x1001_0093 => Some("ATSAMD21G17D"),
            0x6181_0004 => Some("ATSAME51J20A"),
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

    /// Whether this part's NVM User Row is the one [`Samd21UserRowAccess`] reads and rewrites: a
    /// SAM D21 or DA1 (series 0x1), whose row DS40001882D Table 10-4 lays out.
    ///
    /// The SAM D10 and D11 share the flash controller, and their user rows are left to their own
    /// datasheets rather than assumed to match this one.
    pub fn has_samd21_user_row(&self) -> bool {
        self.processor == 0x1 && self.family == 0x0 && self.series == 0x1
    }

    /// Whether this part's flash controller is the one the [`Same54Flash`] routines drive: the
    /// SAM D5x/E5x NVMCTRL, which erases by 8 KiB block and puts its command register where the
    /// D21 puts its configuration register.
    ///
    /// **Measured, not tabulated.** DS60001507 documents what the `DID` FIELDS mean but publishes
    /// no table of their values, so this is the identity read off a SAM E54 Xplained Pro:
    /// `DID 0x61840300`, processor `0x6`, family `0x3`, series `0x4`.
    ///
    /// Series `0x1` (E51) and `0x4` (E54) are claimed. `SERIES` names the 51/53/54 in the ordering
    /// code and selects no controller, and the family has one NVMCTRL chapter with no per-device
    /// qualifier -- but the entry here is each die that has been driven through these routines on a
    /// board, not each die the chapter covers. D51 and E53 are absent for that reason alone.
    ///
    /// # A caveat this predicate cannot express, for any part it claims
    ///
    /// **A D5x/E5x part can report a clean write that did not happen.** A SAM E51 has been observed
    /// to erase correctly, load its page buffer, report `DONE` with no error bit, and read back ALL
    /// ZEROS. The cause is not known, and the signature has not recurred under any condition tried.
    /// **An unexplained fault that has gone quiet is not a fault that has gone.**
    ///
    /// So verify what you write. [`Same54Flash::write_flash`] does NOT -- it reports the
    /// controller's error bits, and those were clean throughout the episode. The flash route reads
    /// the image back and compares it, which is what turns that signature into a mismatch rather
    /// than a silent wrong write, and is why **a D5x/E5x route must never be given a skip-verify
    /// policy.**
    pub fn drives_same54_nvmctrl(&self) -> bool {
        self.processor == 0x6 && self.family == 0x3 && matches!(self.series, 0x1 | 0x4)
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
const SAME54_INTFLAG_DONE: u16 = 1 << 0;
const SAME54_INTFLAG_ADDRE: u16 = 1 << 1;
const SAME54_INTFLAG_PROGE: u16 = 1 << 2;
const SAME54_INTFLAG_LOCKE: u16 = 1 << 3;
const SAME54_INTFLAG_NVME: u16 = 1 << 6;
/// Every way the controller reports that it refused or failed a command.
///
/// NOTE that the ECC flags (`ECCSE` bit 4, `ECCDE` bit 5) are deliberately NOT here. They report on
/// a READ rather than on the command just issued, they clear by reading `ECCERR` rather than by
/// writing a one, and DS60001507 states that ECC errors from debugger reads are not logged in
/// INTFLAG at all -- so treating them as a command result would be wrong in both directions.
const SAME54_COMMAND_ERRORS: u16 =
    SAME54_INTFLAG_ADDRE | SAME54_INTFLAG_PROGE | SAME54_INTFLAG_LOCKE | SAME54_INTFLAG_NVME;
const SAME54_WMODE_MASK: u16 = 0b11 << 4;

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
        same54_clear_errors(self)?;
        self.write_word(SAME54_ADDR, address & !(SAME54_BLOCK - 1))?;
        same54_command(self, SAME54_CMD_EB)?;
        same54_errors(self)
    }

    /// Manual write, per the datasheet's manual-page-write procedure: set WMODE = MAN,
    /// then per page: clear the page buffer, fill it through the flash address space,
    /// set the page address, Write-Page.
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        same54_select_manual_write(self)?;
        same54_clear_errors(self)?;
        for (page, chunk) in words.chunks(SAME54_PAGE / 4).enumerate() {
            let page_addr = address + (page as u32) * SAME54_PAGE as u32;
            same54_ready(self)?;
            same54_command(self, SAME54_CMD_PBC)?;
            self.write_words(page_addr, chunk)?;
            self.write_word(SAME54_ADDR, page_addr)?;
            same54_command(self, SAME54_CMD_WP)?;
        }
        same54_errors(self)
    }
}

/// Polls STATUS.READY (the controller accepts a new command).
///
/// Through the 32-bit word at INTFLAG, which spans INTFLAG and STATUS: both are readable, neither
/// read has a side effect, and the pair costs one probe round trip where two 16-bit reads cost two.
/// This is the hot path -- it runs after every command of every page -- so the pair read stays.
fn same54_ready<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    for _ in 0..1000 {
        if target.read_word(SAME54_INTFLAG)? & SAME54_STATUS_READY != 0 {
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("SAME54 flash controller ready"))
}

/// Puts the controller in MANUAL write mode and CONFIRMS the mode took.
///
/// # Why the read-back is not redundant
///
/// `CTRLA` is a 16-bit register with two reserved bytes after it, so it is written at 16 bits --
/// DS60001507 25.8. And `WMODE` decides whether filling the page buffer merely fills it or commits
/// as it goes, so a write that does not land leaves the mode wherever the resident application put
/// it, which is a property of the board rather than of anything this requests.
///
/// A refusal of that write need not appear in `INTFLAG` at all: `CTRLA` carries the PAC
/// Write-Protection property, and a Peripheral Access Controller refusal is raised there rather
/// than in the NVMCTRL -- a register this driver does not read. Reading `CTRLA` back turns "manual
/// mode was requested" into "the controller is in manual mode", for one round trip per call to
/// [`Same54Flash::write_flash`] rather than per page or per word.
fn same54_select_manual_write<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    let ctrla = target.read_halfword(SAME54_CTRLA)?;
    target.write_halfword(SAME54_CTRLA, ctrla & !SAME54_WMODE_MASK)?;
    let after = target.read_halfword(SAME54_CTRLA)?;
    if after & SAME54_WMODE_MASK != 0 {
        return Err(ProbeError::Device("NVMCTRL CTRLA.WMODE did not take -- the controller is not in manual write mode"));
    }
    Ok(())
}

/// Issues an NVMCTRL command (CMDEX key + `cmd` into CTRLB), waits for ready, and CHECKS WHETHER
/// THE CONTROLLER ACTUALLY DID IT.
///
/// # Readiness is not success, and for a long time this returned as though it were
///
/// `STATUS.READY` says the controller will accept a new command. A controller that REFUSED the last
/// one is also ready -- so polling readiness alone returns `Ok` identically for a command that
/// erased a block and one that did nothing, and the first sign of trouble is a verify mismatch
/// somewhere else that names no cause. `INTFLAG` is where the refusal is stated, and nothing read
/// it.
///
/// The flags are cleared BEFORE the command rather than only read after, because they are sticky
/// (write-one-to-clear) and would otherwise let an error raised by some earlier command be reported
/// against this one -- a false failure being no better than the false success it replaces.
fn same54_command<A: TargetAccess>(target: &mut A, cmd: u32) -> Result<(), ProbeError> {
    target.write_word(SAME54_CTRLB, SAME54_CMDEX | cmd)?;
    same54_ready(target)
}

/// Clears the sticky command-error flags, so what [`same54_errors`] finds afterward belongs to the
/// operation about to run and not to some earlier one.
///
/// # A 16-bit write, because the register above STATUS is where the bits live
///
/// The bits are write-one-to-clear and they belong to `INTFLAG`, which is 16 bits wide
/// (DS60001507 25.8). The next register is `STATUS`, which is read-only, so a 32-bit write here
/// spans a writable register and a read-only one -- and how a bus decomposes that write is not
/// something the register table states. Writing the register that owns the bits avoids the
/// question rather than relying on an answer to it.
fn same54_clear_errors<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    target.write_halfword(SAME54_INTFLAG, SAME54_COMMAND_ERRORS | SAME54_INTFLAG_DONE)
}

/// Whether the controller refused or failed anything since [`same54_clear_errors`].
///
/// # Readiness is not success, and this path reported it as though it were
///
/// `STATUS.READY` says the controller will accept a new command, and a controller that REFUSED the
/// last one is also ready. So polling readiness alone returns `Ok` identically for an erase that
/// cleared a block and one that did nothing, and the first sign of trouble is a verify mismatch
/// somewhere else that names no cause. `INTFLAG` is where the refusal is stated, and nothing read
/// it.
///
/// Checked once per OPERATION rather than once per command, which is what the flags being sticky
/// buys: an error raised by any page of a multi-page write is still set at the end of it. Per
/// command it would have cost two extra round trips against every 512-byte page, on a path whose
/// round-trip count that has already had to be fixed once.
fn same54_errors<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    let flags = target.read_halfword(SAME54_INTFLAG)?;
    match flags & SAME54_COMMAND_ERRORS {
        0 => Ok(()),
        f if f & SAME54_INTFLAG_LOCKE != 0 => Err(ProbeError::Device("NVMCTRL refused: region locked")),
        f if f & SAME54_INTFLAG_ADDRE != 0 => Err(ProbeError::Device("NVMCTRL refused: address error")),
        f if f & SAME54_INTFLAG_PROGE != 0 => Err(ProbeError::Device("NVMCTRL: programming error")),
        _ => Err(ProbeError::Device("NVMCTRL: NVM error")),
    }
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
/// EPA's `FARG[1:0]` = 1 selects 8 pages (4 KiB) -- the one block size legal in BOTH the
/// small 8 KB sectors (which forbid 16/32) and the 48/64 KB sectors (which forbid 4).
const SAM4S_EPA_8_PAGES: u32 = 1;
/// Pages per [`Sam4sFlash::sam4s_erase_pages8`] erase (the EPA 8-page block).
pub const SAM4S_ERASE_PAGES: u32 = 8;
/// Pages in one SAM4S lock region: 16 of 512 bytes, so bit `n` of
/// [`Sam4sFlash::sam4s_lock_bits`] covers pages `16n` to `16n + 15`.
///
/// **IT IS NOT [`SAM3X_LOCK_PAGES`]**, which is 64 pages of 256 bytes. The two families share the
/// register, the `GLB` command and the bit-per-region layout, and a region on one covers 8 KB where
/// a region on the other covers 16.
pub const SAM4S_LOCK_PAGES: u32 = 16;

/// The SAM4E's single EEFC user-interface base -- Atmel-11157 figure 7-1 (product mapping) puts
/// EEFC at 0x400E0A00, with the 0x400E0C00 slot its dual-plane SAM4S sibling uses marked reserved.
pub const SAM4E_EEFC: u32 = SAM4S_EEFC0;
/// The SAM4E's flash window -- Atmel-11157 figure 7-1: Internal Flash at 0x00400000, one plane of
/// 1024 KB on a SAM4E16 and 512 KB on a SAM4E8. Also mirrored at 0x0 when GPNVM1 selects flash boot.
pub const SAM4E_FLASH_BASE: u32 = SAM4S_FLASH0_BASE;
/// The SAM4N's single EEFC user-interface base -- Atmel-11158 section 19.5 gives EEFC_FMR the
/// absolute address 0x400E0A00 at offset 0x00.
pub const SAM4N_EEFC: u32 = SAM4S_EEFC0;
/// The SAM4N's flash window -- Atmel-11158 section 7.1 (product mapping): Internal Flash at
/// 0x00400000, one plane of 1024 KB on a SAM4N16 and 512 KB on a SAM4N8.
pub const SAM4N_FLASH_BASE: u32 = SAM4S_FLASH0_BASE;

/// `CHIPID_CIDR` -- the same address on every SAM4 family.
pub const SAM4_CHIPID_CIDR: u32 = 0x400e_0740;
/// `CHIPID_EXID`, the word after it.
pub const SAM4_CHIPID_EXID: u32 = SAM4_CHIPID_CIDR + 4;

/// A SAM4 part named by its CHIPID pair.
///
/// Deliberately carries NO flash size. The size is what [`Sam4sFlash::sam4s_flash_descriptor`]
/// reports, and a second copy here would manufacture the disagreement the descriptor exists to
/// settle -- on a SAM4E it would also be a figure the CHIPID cannot supply in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sam4Part {
    /// The CSP family name: `sam4e`, `sam4n`, `sam4s`.
    pub family: &'static str,
    /// The part, as its datasheet's chip-id table spells it.
    pub part: &'static str,
}

/// Every SAM4 this crate can name: `(CIDR masked of its revision nibble, EXID, family, part)`.
///
/// **THE FAMILIES VARY IN OPPOSITE FIELDS, AND ONE LOOKUP COVERS BOTH.** A SAM4E gives all four
/// members the same CIDR and separates them by EXID (Atmel-11157 table 32-1); a SAM4N gives each
/// member a DIFFERENT CIDR and reads EXID as zero on every one (Atmel-11158 table 26-1). Matching
/// on the PAIR is what lets one table hold both without either family's rule being a special case.
const SAM4_PARTS: &[(u32, u32, &str, &str)] = &[
    (0xa3cc_0ce0, 0x0012_0200, "sam4e", "ATSAM4E16E (144-pin)"),
    (0xa3cc_0ce0, 0x0012_0208, "sam4e", "ATSAM4E8E (144-pin)"),
    (0xa3cc_0ce0, 0x0012_0201, "sam4e", "ATSAM4E16C (100-pin)"),
    (0xa3cc_0ce0, 0x0012_0209, "sam4e", "ATSAM4E8C (100-pin)"),
    (0x2946_0ce0, 0x0, "sam4n", "ATSAM4N16B"),
    (0x2956_0ce0, 0x0, "sam4n", "ATSAM4N16C"),
    (0x293b_0ae0, 0x0, "sam4n", "ATSAM4N8A"),
    (0x294b_0ae0, 0x0, "sam4n", "ATSAM4N8B"),
    (0x295b_0ae0, 0x0, "sam4n", "ATSAM4N8C"),
    (0x29a7_0ee0, 0x0, "sam4s", "ATSAM4SD32C"),
    (0xab0b_0ae0, 0x1200_0002, "sam4l", "ATSAM4LS8 (512 KB, 48-pin)"),
    (0xab0a_09e0, 0x0200_0002, "sam4l", "ATSAM4LS4 (256 KB, 48-pin)"),
    (0xab0a_07e0, 0x0200_0002, "sam4l", "ATSAM4LS2 (128 KB, 48-pin)"),
];

/// Names the part behind a `CHIPID_CIDR`/`CHIPID_EXID` pair, ignoring the CIDR's revision nibble.
///
/// `None` means "not a SAM4 this crate knows" -- an answer to REFUSE on rather than proceed past,
/// because a flash routine pointed at an unknown die is the one case where guessing costs somebody
/// else's board.
#[must_use]
pub fn sam4_identify(cidr: u32, exid: u32) -> Option<Sam4Part> {
    SAM4_PARTS
        .iter()
        .find(|(id, ex, _, _)| *id == cidr & !0xf && *ex == exid)
        .map(|(_, _, family, part)| Sam4Part { family, part })
}

/// Whether `cidr` belongs to `family` at all, ignoring which member it is.
///
/// The weaker of the two checks, and the right one for a FAMILY-level claim: the sequence
/// [`Sam4sFlash`] drives is a family fact, while the geometry that varies between members comes
/// from the descriptor. A tool demanding an exact member would refuse a sibling it can drive.
#[must_use]
pub fn sam4_family_matches(cidr: u32, family: &str) -> bool {
    SAM4_PARTS.iter().any(|(id, _, fam, _)| *id == cidr & !0xf && *fam == family)
}

/// The first words of the flash descriptor a GETD command returns -- the live geometry
/// cross-check before any erase.
///
/// # `size` IS THIS CONTROLLER'S PLANE, NOT THE DEVICE'S FLASH, AND THE TABLE ALONE DOES NOT SAY SO
///
/// Atmel-11100 table 20-3 names word 1 "Flash size in bytes" and word 4 "Number of bytes in the
/// plane", which reads as though a dual-plane part reports both planes in word 1 and one of them in
/// word 4. It does not, and the scope is fixed one section earlier. Section 20.4.1 opens *"The
/// embedded Flash is composed of: One memory plane organized in several pages of the same size for
/// the code"* and closes *"The EEFC returns a descriptor of the Flash controller after a `Get Flash
/// Descriptor' command has been issued"*. **One descriptor describes one controller, and a
/// controller fronts one plane** -- so within a descriptor, word 1 and word 4 are the same number
/// and `planes` is 1, on every part this crate drives.
///
/// A dual-plane ATSAM4SD32 is two controllers with a user interface each ([`SAM4S_EEFC0`] /
/// [`SAM4S_EEFC1`], peripheral IDs 6 and 7), not one controller reporting two planes.
///
/// The sentence immediately before that one -- *"the embedded Flash size, the page size, the
/// organization of lock regions and the definition of GPNVM bits are specific to the device"* --
/// is about VALUES VARYING BETWEEN PARTS and is the reason every geometry fact here is read from
/// the part rather than from a constant. It is not a statement that one controller reports
/// another's flash.
///
/// The sibling family settles it in the same words: Atmel-11057 section 18.4.1 gives the SAM3X the
/// identical one-plane-per-controller wording, and an ATSAM3X8E -- 512 KB in two 256 KB planes --
/// answers GETD on each of its two controllers with 256 KiB and `planes` = 1. Neither controller
/// reports the device's 512 KB. See [`Sam3xFlashDescriptor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sam4sFlashDescriptor {
    /// FL_ID: flash interface description.
    pub interface: u32,
    /// FL_SIZE: the size in bytes of the plane THIS controller fronts -- see the scope note on the
    /// struct, because the datasheet table calls this one "Flash size".
    pub size: u32,
    /// FL_PAGE_SIZE: page size in bytes (512 on the SAM4S).
    pub page_size: u32,
    /// FL_NB_PLANE: the number of planes this controller fronts, which is 1 on every part this
    /// crate drives -- INCLUDING each half of a dual-plane one. A part answering anything else is
    /// reporting a geometry no routine here is written for, which is why every caller refuses on
    /// it rather than scaling by it.
    pub planes: u32,
    /// FL_PLANE[0]: bytes in this controller's plane.
    ///
    /// **THE SAME NUMBER AS `size` ON EVERY PART THIS CRATE DRIVES, AND READ ANYWAY.** Taking the
    /// field that MEANS a plane is what stops a walk being bounded by a number that only happens
    /// to be a plane's -- and the descriptor carries it, so reading it costs one FRR word.
    pub plane_bytes: u32,
}

/// SAM4S (ATSAM4S / SAM4SD dual-plane) flash programming, added to a CMSIS-DAP [`TargetAccess`]
/// probe. `eefc` selects the controller ([`SAM4S_EEFC0`] / [`SAM4S_EEFC1`]); page numbers
/// are relative to that controller's plane. Halt the core before erasing or writing.
///
/// # It is the SAM4E's and SAM4N's sequence too, read in three datasheets
///
/// Every fact this routine uses was looked up in each part's OWN document -- Atmel-11157 for the
/// SAM4E, Atmel-11158 for the SAM4N -- rather than carried across from the SAM4S's Atmel-11100,
/// because the EEFC chapter states outright that "the embedded Flash size, the page size, the
/// organization of lock regions and the definition of GPNVM bits are specific to the device":
///
/// ```text
/// EEFC user interface  0x400E0A00        all three, and the register map is FMR/FCR/FSR/FRR
///                                        at 0x00/0x04/0x08/0x0C on all three
/// flash window         0x00400000        all three
/// FCR layout           FKEY 0x5A in [31:24], FARG in [23:8], FCMD in [7:0]      all three
/// command values       GETD 0x00, WP 0x01, EPA 0x07, CLB 0x09, GLB 0x0A,
///                      SGPB 0x0B, CGPB 0x0C, GGPB 0x0D, STUI 0x0E, SPUI 0x0F    all three
/// FSR bits             FRDY 0, FCMDE 1, FLOCKE 2, FLERR 3                       all three
/// write unit           a 512-byte PAGE, filled through a latch buffer in the flash window
/// erase                EPA with FARG[1:0] = 1 -- an 8-page block, and it is the one code
///                      every sector class of all three parts grants
/// lock region          8 KB = 16 pages
/// ```
///
/// **THE 8-PAGE ERASE IS THE INTERESTING ONE, BECAUSE THE THREE DOCUMENTS ARGUE FOR IT
/// DIFFERENTLY.** The SAM4S and SAM4E list the legal codes per sector class -- the 8 KB sectors
/// forbid 16 and 32, the 48/64 KB sectors do not offer 4 -- so 8 pages is the intersection. The
/// SAM4N does not tabulate it that way at all; it grants a 4 KB block "inside a sector of 8
/// Kbytes/48 Kbytes/64 Kbytes" with `FARG[1:0]` = 1 directly. Same code, reached two ways.
///
/// # Two things are SAM4S-only, and a caller on a SAM4E or SAM4N must not reach for them
///
/// - **[`SAM4S_EEFC1`], [`SAM4S_FLASH1_BASE`] and [`SAM4S_GPNVM_PLANE_SWAP`] DO NOT EXIST on
///   either part.** Both are single-plane and both datasheets say so in a sentence -- "the Flash of
///   SAM4E is composed of 1024 Kbytes in a single bank", and each "features two GPNVM bits", 0
///   security and 1 boot mode. There is no bit 2 to swap and no second window to swap it with, so
///   [`Sam4sFlash::sam4s_set_gpnvm`] with `SAM4S_GPNVM_PLANE_SWAP` is a command the part will
///   refuse rather than a plane switch. Use [`SAM4E_EEFC`] / [`SAM4E_FLASH_BASE`] and
///   [`SAM4N_EEFC`] / [`SAM4N_FLASH_BASE`], which name what is actually there.
/// - **The dummy read after a GPNVM or lock-bit write is a SAM4S FACT.** It implements the SAM4S
///   erratum "Read Error after a GPNVM or Lock Bit Writing"; neither the SAM4E's errata (section
///   50) nor the SAM4N's (section 40) carries it. It stays in the shared path because one
///   discarded word read cannot make a part that does not need it wrong -- but it is a workaround
///   travelling under a scope claim, not a third part's requirement, and that is worth knowing
///   before anyone deletes it on a SAM4E and calls the SAM4S fixed.
///
/// # One more thing that is NOT constant, and the driver already handles it the right way
///
/// The SAM4N's lock-bit COUNT is per part -- 64 on a SAM4N8, 128 on a SAM4N16 (Atmel-11158 table
/// 7-1) -- while [`Sam4sFlash::sam4s_lock_bits`] always reads four words. That is safe rather than
/// lucky: the datasheet says extra reads of `EEFC_FRR` return 0, so a SAM4N8 answers two real words
/// and two zeros. A zero is "unlocked", which is the truth for a region that does not exist.
///
/// And the SAM4E's `CHIPID_CIDR` cannot tell its members apart at all: Atmel-11157 table 32-1 gives
/// **all four** of the SAM4E16E/8E/16C/8C the same `0xA3CC_0CE0`, with only `CHIPID_EXID`
/// distinguishing them. So a CIDR check is a FAMILY guard and never a size one -- take the size
/// from [`Sam4sFlash::sam4s_flash_descriptor`], which is the controller reporting its own geometry.
///
/// # Which of the three is MEASURED, because three datasheets agreeing is not the same evidence
///
/// ```text
/// SAM4S   measured   an ATSAM4SD32C, including the dual-plane and GPNVM2 paths
/// SAM4E   measured   an ATSAM4E16E
/// SAM4N   measured   an ATSAM4N16C
/// ```
///
/// The two single-plane runs were the same procedure, `sam4-flashtest`: GETD reported a 512-byte
/// page and ONE plane, then a 4 KB block at the top of the plane erased to ones, programmed,
/// verified word for word against an ADDRESS-DERIVED pattern -- so a word landing in the wrong page
/// mismatches instead of coinciding -- and was restored, with a full 1 MB dump before and after
/// hashing identical on both boards.
///
/// **THE REFUSALS WERE EXERCISED, NOT JUST THE HAPPY PATH.** The page-size and plane-count asserts
/// were each perturbed against a live SAM4E and each stopped the tool before it erased anything;
/// the family guard was then tested for real rather than by perturbation, by pointing the SAM4E
/// tool at the SAM4N, which it refused while naming the part it had actually found. A guard that
/// has only ever passed is a guard nobody has tested.
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
            plane_bytes: self.read_word(eefc + SAM4S_FRR)?,
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

/// Pages in one SAM3X lock region: 16 KB of 256-byte pages, so bit `n` of
/// [`Sam3xFlash::sam3x_lock_bits`] covers pages `64n` to `64n + 63`.
///
/// **THE COUNT IS NOT THE SAM4S's**, whose regions are 16 pages of 512 bytes -- 8 KB. The two
/// families share the register, the command and the bit-per-region layout, and a region on one is
/// twice the flash of a region on the other.
pub const SAM3X_LOCK_PAGES: u32 = 64;

/// `CHIPID_CIDR` on a SAM3X / SAM3A -- Atmel-11057 section 29.3.1, which gives the address in
/// words.
///
/// **IT IS NOT [`SAM4_CHIPID_CIDR`], AND THE TWO ARE 0x200 APART.** A SAM4 answers at
/// `0x400E0740`; reading that address on a SAM3X reads a TWI controller, decodes whatever it finds
/// as a chip id, and refuses or accepts on it. The families share a controller SHAPE and not this.
pub const SAM3X_CHIPID_CIDR: u32 = 0x400e_0940;
/// `CHIPID_EXID`, the word after it (Atmel-11057 section 29.3.2). Reads 0 on every part in
/// [`SAM3X_PARTS`], because `CIDR.EXT` is clear on all of them.
pub const SAM3X_CHIPID_EXID: u32 = SAM3X_CHIPID_CIDR + 4;

/// Every SAM3X / SAM3A this crate can name: `(CIDR masked of its VERSION field, part)`.
///
/// **A THIRD PATTERN AGAIN, AFTER THE SAM4E's AND THE SAM4N's**: here each member has its own CIDR
/// and `CHIPID_EXID` reads zero on all of them (Atmel-11057 table 29-1), so the CIDR alone settles
/// the member and the EXID carries nothing to match on.
///
/// **THE MASK IS FIVE BITS, NOT FOUR.** `CIDR.VERSION` is bits 4:0 on this family and `EPROC` is
/// 7:5 -- so masking a revision off with the SAM4 table's `!0xf` would leave a version bit standing
/// and refuse a later silicon revision of a part that is right here on the list.
const SAM3X_PARTS: &[(u32, &str)] = &[
    (0x285e_0a60, "ATSAM3X8E (144-pin, 2 x 256 KB)"),
    (0x285b_0960, "ATSAM3X4E (144-pin, 2 x 128 KB)"),
    (0x284e_0a60, "ATSAM3X8C (100-pin, 2 x 256 KB)"),
    (0x284b_0960, "ATSAM3X4C (100-pin, 2 x 128 KB)"),
    (0x283e_0a60, "ATSAM3A8C (100-pin, 2 x 256 KB)"),
    (0x283b_0960, "ATSAM3A4C (100-pin, 2 x 128 KB)"),
    (0x286e_0a60, "ATSAM3X8H (217-pin, 2 x 256 KB)"),
];

/// Names the SAM3X / SAM3A behind a `CHIPID_CIDR`, ignoring the version field.
///
/// `None` means "not a SAM3X/A this crate knows" -- an answer to REFUSE on rather than proceed
/// past, for the reason [`sam4_identify`] gives.
#[must_use]
pub fn sam3x_identify(cidr: u32) -> Option<&'static str> {
    SAM3X_PARTS
        .iter()
        .find(|(id, _)| *id == cidr & !0x1f)
        .map(|(_, part)| *part)
}

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
///
/// # `size` IS THIS CONTROLLER'S PLANE, AND ON THIS FAMILY THAT IS A MEASUREMENT RATHER THAN A READING
///
/// Every SAM3X and SAM3A is dual-plane, so this is the family where the wrong reading of the table
/// would show. Atmel-11057 table 18-3 names word 1 "Flash size in bytes" and word 4 "Number of
/// bytes in the first plane"; section 18.4.1 fixes what that is the size OF, in the same words its
/// SAM4S twin uses -- *"The embedded Flash is composed of: One memory plane organized in several
/// pages of the same size"*, and *"The Enhanced Embedded Flash Controller (EEFC) returns a
/// descriptor of the Flash controlled"*.
///
/// **An ATSAM3X8E is 512 KB in two 256 KB planes, fronted by two controllers
/// ([`SAM3X_EEFC0`] / [`SAM3X_EEFC1`], peripheral IDs 6 and 7). GETD on EACH reports
/// `size` = 256 KiB, `page_size` = 256 and `planes` = 1.** Neither controller reports the device's
/// 512 KB, and nothing here should be written as though one might.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sam3xFlashDescriptor {
    /// FL_ID: flash interface description.
    pub interface: u32,
    /// FL_SIZE: the size in bytes of the plane THIS controller fronts -- 256 KiB on an ATSAM3X8E.
    /// See the scope note on the struct, because the datasheet table calls this one "Flash size".
    pub size: u32,
    /// FL_PAGE_SIZE: page size in bytes (256 on the SAM3X).
    pub page_size: u32,
    /// FL_NB_PLANE: the number of planes this controller fronts -- 1 on each of the two, not 2 on
    /// either.
    pub planes: u32,
    /// FL_PLANE[0]: bytes in this controller's plane -- the same number as `size`, read because
    /// it is the field that MEANS a plane. See the SAM4S twin.
    pub plane_bytes: u32,
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
            plane_bytes: self.read_word(eefc + SAM3X_FRR)?,
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

/// The FLASHCALW user interface (Atmel-42023H, Peripheral Bridge B).
pub const SAM4L_FLASHCALW: u32 = 0x400a_0000;
/// The SAM4L flash window. NOT 0x00400000 -- this family maps flash at zero.
pub const SAM4L_FLASH_BASE: u32 = 0x0000_0000;
/// Lock regions, always 16 on this family; each covers `pages / 16` pages (Atmel-42023H 14.6.2).
/// Their status is READ STRAIGHT OUT OF [`SAM4L_FSR`] bits 31:16 -- there is no get-lock-bits
/// command to issue, which is the one place this controller is simpler than an EEFC.
pub const SAM4L_LOCK_REGIONS: u32 = 16;

const SAM4L_FCMD: u32 = 0x04;
const SAM4L_FSR: u32 = 0x08;
const SAM4L_FPR: u32 = 0x0c;
const SAM4L_FKEY: u32 = 0xa5 << 24;
const SAM4L_CMD_WP: u32 = 1;
const SAM4L_CMD_EP: u32 = 2;
const SAM4L_CMD_CPB: u32 = 3;
const SAM4L_CMD_QPR: u32 = 12;
/// PicoCache `MAINT0`, at 0x420 from the FLASHCALW base (the PicoCache block sits at +0x400).
const SAM4L_PICOCACHE_MAINT0: u32 = 0x420;
/// `MAINT0.INVALL`: "when set to one, this field invalidate all cache entries".
const SAM4L_MAINT0_INVALL: u32 = 1 << 0;
const SAM4L_FSR_FRDY: u32 = 1 << 0;
const SAM4L_FSR_LOCKE: u32 = 1 << 2;
const SAM4L_FSR_PROGE: u32 = 1 << 3;
const SAM4L_FSR_SECURITY: u32 = 1 << 4;
const SAM4L_FSR_QPRR: u32 = 1 << 5;
/// FSR bit 16 is lock region 0; region 15 is bit 31.
const SAM4L_FSR_LOCK0_SHIFT: u32 = 16;

/// `FPR.FSZ` -> flash size in bytes (Atmel-42023H table 14-7).
///
/// **A TABLE AND NOT A SHIFT, WHICH IS THE WHOLE REASON IT IS WRITTEN OUT.** The first four codes
/// are 4/8/16/32 KB, so `4096 << FSZ` looks right and keeps looking right through code 3 -- then
/// code 4 is 48 KB rather than 64, and the sequence goes 48, 64, 96, 128, 192, 256, 384, 512, 768,
/// 1024, 2048. A formula fitted to the low codes is wrong on over half the table and silently.
const SAM4L_FLASH_SIZES: [u32; 15] = [
    4, 8, 16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 2048,
];

/// What `FPR` reports about the part in front of you -- the live geometry cross-check, and this
/// family's answer to the EEFC's flash descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sam4lFlashParameters {
    /// Page size in bytes, decoded from `PSZ` (32 << PSZ; 4 is the SAM4L's 512).
    pub page_size: u32,
    /// Flash size in bytes, decoded from `FSZ` through [`SAM4L_FLASH_SIZES`]. `None` for the
    /// reserved code 15, which is reported rather than guessed at.
    pub flash_size: Option<u32>,
    /// The raw register, so a caller can report what it actually saw.
    pub raw: u32,
}

impl Sam4lFlashParameters {
    /// Decodes an `FPR` value.
    #[must_use]
    pub fn from_fpr(fpr: u32) -> Sam4lFlashParameters {
        let psz = (fpr >> 8) & 0x7;
        let fsz = (fpr & 0xf) as usize;
        Sam4lFlashParameters {
            page_size: 32 << psz,
            flash_size: SAM4L_FLASH_SIZES.get(fsz).map(|kb| kb * 1024),
            raw: fpr,
        }
    }

    /// Pages in the array, or `None` when the part reported the reserved size code.
    #[must_use]
    pub fn pages(&self) -> Option<u32> {
        self.flash_size.map(|size| size / self.page_size)
    }
}

/// SAM4L (ATSAM4L8/L4/L2) flash programming over a CMSIS-DAP [`TargetAccess`] probe. Halt the core
/// before erasing or writing so it is not fetching from the array during the operation.
///
/// # The write sequence, and the step that has no EEFC equivalent
///
/// ```text
/// 1. Erase Page          EP with PAGEN = the page
/// 2. Clear Page Buffer   CPB -- MANDATORY, see below
/// 3. fill the buffer     ordinary word writes THROUGH THE FLASH WINDOW; they do not reach the
///                        array, they land in the page buffer and update PAGEN as a side effect
/// 4. Write Page          WP with PAGEN = the page
/// ```
///
/// **STEP 2 IS THE ONE THAT BITES.** The page buffer can only clear bits, and the datasheet says
/// outright it "is not automatically reset after a page write". Omit the clear and the first page
/// written after reset comes out correct while every later page is ANDed with the one before it --
/// a fault that passes a single-page test and corrupts a real image.
///
/// **STEP 3 IS ALSO NOT WHAT IT LOOKS LIKE.** Writing to the flash window here does not write
/// flash. Until `WP` commits, a read back through the same window returns the ARRAY, not what was
/// just written -- so a read-after-write check placed between steps 3 and 4 reads the old contents
/// and looks like a failed write.
///
/// **AND STEP 4 IS NOT THE END: THE READ PATH IS CACHED.** See
/// [`Sam4lFlash::sam4l_invalidate_cache`]. `WP` completing does not make the new bytes visible to a
/// debugger's reads, and this is the one behavior here with no EEFC analogue at all.
///
/// # Proof status
///
/// This sequence is exercised against hardware rather than only read out of Atmel-42023H.
///
/// **A SINGLE-PAGE TEST COULD NOT ESTABLISH IT**, and that is worth knowing before anyone trims the
/// harness: the page buffer starts erased, so an implementation that never issued `Clear Page
/// Buffer` programs its FIRST page correctly and silently corrupts every one after. So the same
/// page is written twice with different contents, and it is the SECOND write that carries the
/// evidence.
pub trait Sam4lFlash {
    /// Reads `FPR` and decodes it -- the geometry, from the part rather than from a table here.
    fn sam4l_flash_parameters(&mut self) -> Result<Sam4lFlashParameters, ProbeError>;
    /// The raw `FSR`. Reading it CLEARS `LOCKE`, `PROGE` and `ECCERR`, so a caller that wants those
    /// must read this once and inspect the value, not read it twice.
    fn sam4l_status(&mut self) -> Result<u32, ProbeError>;
    /// The 16 region lock bits, from `FSR` bits 31:16. Bit n set = region n locked.
    fn sam4l_lock_bits(&mut self) -> Result<u16, ProbeError>;
    /// Whether the security fuses report a protected state (`FSR.SECURITY`). A protected part
    /// refuses debug access to flash, and NOTHING in this trait can clear it -- that needs the
    /// external erase path.
    fn sam4l_is_secure(&mut self) -> Result<bool, ProbeError>;
    /// Invalidates every PicoCache entry, via `MAINT0.INVALL`.
    ///
    /// # This family has a cache in front of flash, and a debugger's reads go through it
    ///
    /// **MEASURED, AFTER IT COST A WRONG CONCLUSION.** A page was erased and programmed, `QPR`
    /// confirmed on the part that the page held content -- and reading the same address over the
    /// MEM-AP returned 0xFFFFFFFF, indefinitely, across a fresh probe attach. The first reading of
    /// that evidence is "the write silently failed", and it is wrong: a `SYSRESETREQ` made the
    /// written bytes appear immediately. The array was correct the whole time and the READ was
    /// stale.
    ///
    /// No EEFC part in this crate behaves this way, which is exactly why it is easy to be caught
    /// by: every habit built on the SAM4E, SAM4N and SAM4S says a read-back after a program is the
    /// verification. Here it is only a verification once the cache has been invalidated.
    ///
    /// [`Sam4lFlash::sam4l_erase_page`] and [`Sam4lFlash::sam4l_write_page`] both call this, on the
    /// principle that whoever invalidated the DATA should invalidate the CACHE. It is public
    /// because a caller writing through some other path still needs it.
    fn sam4l_invalidate_cache(&mut self) -> Result<(), ProbeError>;
    /// Erases one page, via `EP`, then invalidates the cache.
    fn sam4l_erase_page(&mut self, page: u32) -> Result<(), ProbeError>;
    /// Asks the CONTROLLER whether a page reads erased, via `QPR` -- it ANDs every bit in the page
    /// and reports one bit in `FSR.QPRR`. Cheaper than reading the page over the wire, and it is
    /// the part's own opinion rather than the host's.
    fn sam4l_page_is_erased(&mut self, page: u32) -> Result<bool, ProbeError>;
    /// Clears the page buffer, via `CPB`. Called by [`Sam4lFlash::sam4l_write_page`]; exposed
    /// because a caller filling the buffer itself still needs it.
    fn sam4l_clear_page_buffer(&mut self) -> Result<(), ProbeError>;
    /// Clears the page buffer, fills it with `words`, and commits with `WP`.
    ///
    /// `words` must be exactly one page. A short slice is REFUSED rather than padded: the erased
    /// value is 0xFFFFFFFF and padding with it would be correct, but the page buffer holds whatever
    /// the last fill left unless cleared, so "the caller meant to write less" and "the caller
    /// computed the page size wrong" are the same call, and only one of them is safe.
    fn sam4l_write_page(
        &mut self,
        page: u32,
        page_size: u32,
        words: &[u32],
    ) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Sam4lFlash for A {
    fn sam4l_flash_parameters(&mut self) -> Result<Sam4lFlashParameters, ProbeError> {
        Ok(Sam4lFlashParameters::from_fpr(
            self.read_word(SAM4L_FLASHCALW + SAM4L_FPR)?,
        ))
    }

    fn sam4l_status(&mut self) -> Result<u32, ProbeError> {
        self.read_word(SAM4L_FLASHCALW + SAM4L_FSR)
    }

    fn sam4l_lock_bits(&mut self) -> Result<u16, ProbeError> {
        Ok((self.sam4l_status()? >> SAM4L_FSR_LOCK0_SHIFT) as u16)
    }

    fn sam4l_is_secure(&mut self) -> Result<bool, ProbeError> {
        Ok(self.sam4l_status()? & SAM4L_FSR_SECURITY != 0)
    }

    fn sam4l_invalidate_cache(&mut self) -> Result<(), ProbeError> {
        self.write_word(
            SAM4L_FLASHCALW + SAM4L_PICOCACHE_MAINT0,
            SAM4L_MAINT0_INVALL,
        )
    }

    fn sam4l_erase_page(&mut self, page: u32) -> Result<(), ProbeError> {
        sam4l_command(self, SAM4L_CMD_EP, page)?;
        self.sam4l_invalidate_cache()
    }

    fn sam4l_page_is_erased(&mut self, page: u32) -> Result<bool, ProbeError> {
        sam4l_command(self, SAM4L_CMD_QPR, page)?;
        Ok(self.sam4l_status()? & SAM4L_FSR_QPRR != 0)
    }

    fn sam4l_clear_page_buffer(&mut self) -> Result<(), ProbeError> {
        sam4l_command(self, SAM4L_CMD_CPB, 0)
    }

    fn sam4l_write_page(
        &mut self,
        page: u32,
        page_size: u32,
        words: &[u32],
    ) -> Result<(), ProbeError> {
        if words.len() * 4 != page_size as usize {
            return Err(ProbeError::Device(
                "SAM4L write page: the slice is not exactly one page",
            ));
        }
        self.sam4l_clear_page_buffer()?;
        self.write_words(SAM4L_FLASH_BASE + page * page_size, words)?;
        sam4l_command(self, SAM4L_CMD_WP, page)?;
        self.sam4l_invalidate_cache()
    }
}

/// Issues one FLASHCALW command (key | pagen | cmd into FCMD) and waits for FRDY, mapping the FSR
/// error flags: `PROGE` = bad key or invalid command, `LOCKE` = the command hit a locked region and
/// was refused.
///
/// Both error bits are CLEARED BY THE READ that observes them, so they are read once into `fsr` and
/// inspected there. Reading FSR a second time to re-check would always find them clear.
fn sam4l_command<A: TargetAccess>(target: &mut A, cmd: u32, pagen: u32) -> Result<(), ProbeError> {
    target.write_word(
        SAM4L_FLASHCALW + SAM4L_FCMD,
        SAM4L_FKEY | ((pagen & 0xffff) << 8) | cmd,
    )?;
    for _ in 0..4000 {
        let fsr = target.read_word(SAM4L_FLASHCALW + SAM4L_FSR)?;
        if fsr & SAM4L_FSR_FRDY != 0 {
            if fsr & SAM4L_FSR_PROGE != 0 {
                return Err(ProbeError::Device("SAM4L FLASHCALW programming error (PROGE)"));
            }
            if fsr & SAM4L_FSR_LOCKE != 0 {
                return Err(ProbeError::Device("SAM4L FLASHCALW lock violation (LOCKE)"));
            }
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("SAM4L flash controller (FRDY)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cmsis_dap::{Dap, proto};
    use lamella_probe_core::ArmDap;
    use lamella_cmsis_dap::testing::{Mock, echo};

    /// The SAM D10 and SAM D11 are inside this driver's scope only while these three facts hold,
    /// and each is read from that part's own datasheet. If a later part joins the family with a
    /// different page, this is what refuses it rather than a silent half-programmed row.
    #[test]
    fn the_d10_and_d11_share_every_fact_this_driver_uses() {
        assert_eq!(SAMD21_CTRLA, 0x4100_4000);
        assert_eq!(SAMD21_CTRLB, SAMD21_CTRLA + 0x04);
        assert_eq!(SAMD21_INTFLAG, SAMD21_CTRLA + 0x14);
        assert_eq!(SAMD21_ADDR, SAMD21_CTRLA + 0x1c);
        assert_eq!(SAMD21_PAGE, 64);
        assert_eq!(SAMD21_ROW as usize, SAMD21_PAGE * 4);
    }

    /// The SAM4E and SAM4N ride [`Sam4sFlash`] only while these facts hold, and every literal on
    /// the right was read in that part's own datasheet -- Atmel-11157 and Atmel-11158 -- rather
    /// than in the SAM4S's. What this refuses is the failure the aliases would otherwise allow: a
    /// later edit moving the SHARED value to suit one family, silently taking the others with it.
    #[test]
    fn the_sam4e_and_sam4n_share_every_fact_this_driver_uses() {
        assert_eq!(SAM4E_EEFC, 0x400e_0a00);
        assert_eq!(SAM4N_EEFC, 0x400e_0a00);
        assert_eq!(SAM4E_FLASH_BASE, 0x0040_0000);
        assert_eq!(SAM4N_FLASH_BASE, 0x0040_0000);
        assert_eq!(SAM4S_FCR, 0x04);
        assert_eq!(SAM4S_FSR, 0x08);
        assert_eq!(SAM4S_FRR, 0x0c);
        assert_eq!(SAM4S_FKEY, 0x5a << 24);
        assert_eq!(
            [
                SAM4S_CMD_GETD, SAM4S_CMD_WP, SAM4S_CMD_EPA, SAM4S_CMD_CLB, SAM4S_CMD_GLB,
                SAM4S_CMD_SGPB, SAM4S_CMD_CGPB, SAM4S_CMD_GGPB, SAM4S_CMD_STUI, SAM4S_CMD_SPUI,
            ],
            [0x00, 0x01, 0x07, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f],
        );
        assert_eq!(
            [SAM4S_FSR_FRDY, SAM4S_FSR_FCMDE, SAM4S_FSR_FLOCKE, SAM4S_FSR_FLERR],
            [1 << 0, 1 << 1, 1 << 2, 1 << 3],
        );
        assert_eq!(SAM4S_PAGE, 512);
        assert_eq!(SAM4S_EPA_8_PAGES, 1);
        assert_eq!(SAM4S_ERASE_PAGES, 8);
        assert_eq!(8 * 1024 / SAM4S_PAGE, 16);
    }

    /// The two families identify themselves in OPPOSITE fields, so a lookup that reads only one
    /// register would name every SAM4E member wrong or every SAM4N member wrong depending which
    /// it picked. Both directions are exercised here, from each part's own chip-id table.
    #[test]
    fn sam4_identity_reads_cidr_and_exid_together() {
        let e16e = sam4_identify(0xa3cc_0ce0, 0x0012_0200).expect("the measured SAM4E16E");
        assert_eq!(e16e.family, "sam4e");
        assert_eq!(e16e.part, "ATSAM4E16E (144-pin)");
        assert_eq!(sam4_identify(0xa3cc_0ce0, 0x0012_0208).unwrap().part, "ATSAM4E8E (144-pin)");
        assert_eq!(sam4_identify(0x2956_0ce0, 0x0).unwrap().part, "ATSAM4N16C");
        assert_eq!(sam4_identify(0x293b_0ae0, 0x0).unwrap().part, "ATSAM4N8A");
        assert_eq!(sam4_identify(0xa3cc_0ce1, 0x0012_0200), Some(e16e));
        assert_eq!(sam4_identify(0xa3cc_0ce0, 0x0), None);
        assert!(sam4_family_matches(0xa3cc_0ce0, "sam4e"));
        assert!(sam4_family_matches(0x2956_0ce0, "sam4n"));
        assert!(!sam4_family_matches(0x2956_0ce0, "sam4e"));
        assert!(!sam4_family_matches(0xa3cc_0ce0, "sam4n"));
        assert!(!sam4_family_matches(0xdead_beef, "sam4e"));
        assert_eq!(sam4_identify(0xdead_beef, 0x0), None);
    }

    /// The FLASHCALW is not an EEFC, and the three constants that would be copied across are the
    /// three that differ in a way no compiler and no test but this one would catch. Each is
    /// asserted AGAINST its EEFC counterpart rather than only against its own literal, because a
    /// bare literal check passes just as happily after somebody pastes the wrong value in.
    #[test]
    fn the_flashcalw_constants_are_not_the_eefcs() {
        assert_eq!(SAM4L_FKEY, 0xa5 << 24);
        assert_ne!(SAM4L_FKEY, SAM4S_FKEY);
        assert_eq!(SAM4L_FSR_PROGE, 1 << 3);
        assert_ne!(SAM4L_FSR_PROGE, SAM4S_FSR_FCMDE);
        assert_eq!(SAM4L_FSR_PROGE, SAM4S_FSR_FLERR);
        assert_eq!(SAM4L_FSR_FRDY, SAM4S_FSR_FRDY);
        assert_eq!(SAM4L_FSR_LOCKE, SAM4S_FSR_FLOCKE);
        assert_eq!(SAM4L_FLASH_BASE, 0);
        assert_ne!(SAM4L_FLASH_BASE, SAM4S_FLASH0_BASE);
        assert_eq!(SAM4L_FLASHCALW, 0x400a_0000);
        assert_ne!(SAM4L_FLASHCALW, SAM4S_EEFC0);
    }

    /// `FPR` is this family's flash descriptor, and `FSZ` is a LOOKUP rather than a shift. The
    /// first four codes make `4096 << FSZ` look correct; the table then stops being a power-of-two
    /// ladder, so the codes that would expose the shortcut are the ones checked hardest.
    #[test]
    fn the_flash_parameter_register_decodes_a_table_not_a_shift() {
        let l8 = Sam4lFlashParameters::from_fpr((4 << 8) | 11);
        assert_eq!(l8.page_size, 512);
        assert_eq!(l8.flash_size, Some(512 * 1024));
        assert_eq!(l8.pages(), Some(1024));
        assert_eq!(Sam4lFlashParameters::from_fpr(4).flash_size, Some(48 * 1024));
        assert_eq!(Sam4lFlashParameters::from_fpr(6).flash_size, Some(96 * 1024));
        assert_eq!(Sam4lFlashParameters::from_fpr(12).flash_size, Some(768 * 1024));
        assert_eq!(Sam4lFlashParameters::from_fpr(0).flash_size, Some(4 * 1024));
        assert_eq!(Sam4lFlashParameters::from_fpr(3).flash_size, Some(32 * 1024));
        assert_eq!(Sam4lFlashParameters::from_fpr(15).flash_size, None);
        assert_eq!(Sam4lFlashParameters::from_fpr(15).pages(), None);
        assert_eq!(Sam4lFlashParameters::from_fpr(0 << 8).page_size, 32);
        assert_eq!(Sam4lFlashParameters::from_fpr(7 << 8).page_size, 4096);
    }

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

    /// The SAM D21 cold-plugging park must REQUEST `SWCLK` low across the release of nRESET, not
    /// inherit it from whatever the probe happens to idle at.
    ///
    /// The old sequence was `set_reset(true)` then `set_reset(false)`, which is also two
    /// `DAP_SWJ_Pins` frames -- so a test counting frames, or checking that nRESET moved, passes on
    /// both. THE SELECT MASK IS THE ONLY DIFFERENCE AND IT IS THE WHOLE MECHANISM: `SWCLK` low as
    /// nRESET releases is what puts the part in the DSU reset extension instead of running its
    /// application.
    ///
    /// The park itself times out here, deliberately -- the mock answers the pin frames and nothing
    /// after them. What is under test is the two frames it opens with.
    #[test]
    fn samd21_park_requests_swclk_low_rather_than_relying_on_it() {
        let replies = vec![
            echo(proto::cmd::SWJ_PINS, &[0x00]),
            echo(proto::cmd::SWJ_PINS, &[proto::PIN_NRESET]),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let _ = target.samd21_park();

        let lines = proto::PIN_NRESET | proto::PIN_SWCLK;
        let sent = &target.inner().transport().sent;
        assert_eq!(sent[0][0], proto::cmd::SWJ_PINS);
        assert_eq!(sent[0][1], 0x00, "both lines driven LOW");
        assert_eq!(sent[0][2], lines, "and SWCLK is SELECTED -- `set_reset` selects nRESET alone");
        assert_eq!(sent[1][1], proto::PIN_NRESET, "nRESET released");
        assert_eq!(sent[1][2], lines, "with SWCLK still selected and still low: the extension");
    }

    /// The reset-halt path arms the vector catch WHILE the core is held, so its two edges cannot
    /// come from one call -- and the release is still the half that must hold `SWCLK` low.
    ///
    /// Three pin frames, not two: `set_reset(true)`, then the extension's own assert (a no-op
    /// against a line already low) and its release.
    #[test]
    fn samd21_reset_halt_releases_through_the_extension() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let replies = vec![
            echo(proto::cmd::SWJ_PINS, &[0x00]),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            echo(proto::cmd::SWJ_PINS, &[0x00]),
            echo(proto::cmd::SWJ_PINS, &[proto::PIN_NRESET]),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let _ = target.samd21_reset_halt();

        let lines = proto::PIN_NRESET | proto::PIN_SWCLK;
        let pins: Vec<&Vec<u8>> =
            target.inner().transport().sent.iter().filter(|f| f[0] == proto::cmd::SWJ_PINS).collect();
        assert!(pins.len() >= 3, "assert, then the extension's two edges");
        assert_eq!(pins[0][2], proto::PIN_NRESET, "the hold selects nRESET alone, as it always did");
        assert_eq!(pins[1][1], 0x00, "the extension re-asserts against an already-low line");
        assert_eq!(pins[1][2], lines);
        assert_eq!(pins[2][1], proto::PIN_NRESET, "and RELEASES nRESET...");
        assert_eq!(pins[2][2], lines, "...with SWCLK selected low, which is the half that matters");
    }

    /// STATUS.READY set, seen through the 32-bit read at INTFLAG (+0x10): bit 16.
    fn same54_ready_reply() -> Vec<u8> {
        vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00]
    }

    /// `n` plain transfer acknowledges, for the steps a test is not asserting on.
    fn acks(n: usize) -> Vec<Vec<u8>> {
        (0..n).map(|_| echo(proto::cmd::TRANSFER, &[0x01, 0x01])).collect()
    }

    /// A single-access `DAP_Transfer` decoded far enough to say what it did to which AP register.
    ///
    /// The tests below used to index `sent` by position, which pinned the transfer COUNT as much as
    /// the behavior -- so a change in ACCESS WIDTH, which adds two CSW writes around an access,
    /// turned every assertion in them red at once while telling you nothing about what changed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Xfer {
        /// The MEM-AP register: 0x00 CSW, 0x04 TAR, 0x0c DRW.
        reg: u8,
        read: bool,
        /// The value written; meaningless for a read.
        value: u32,
    }

    /// Decodes the single-access transfers out of a sent-packet log, ignoring block transfers.
    ///
    /// The request byte is ADIv5's: bit 0 selects AP over DP, bit 1 is read-not-write, and bits 3:2
    /// are `A[3:2]` -- the register selector's top two bits, so 0b00/0b01/0b11 name CSW/TAR/DRW.
    fn transfers(sent: &[Vec<u8>]) -> Vec<Xfer> {
        sent.iter()
            .filter(|f| f[0] == proto::cmd::TRANSFER && f.len() >= 4)
            .map(|f| {
                let request = f[3];
                let read = request & 0b10 != 0;
                let value = if f.len() >= 8 {
                    u32::from_le_bytes([f[4], f[5], f[6], f[7]])
                } else {
                    0
                };
                Xfer { reg: (request >> 2) & 0b11, read, value }
            })
            .map(|x| Xfer { reg: [0x00, 0x04, 0x08, 0x0c][x.reg as usize], ..x })
            .collect()
    }

    const CSW_WORD: u32 = 0x2300_0052;
    const CSW_HALF: u32 = 0x2300_0041;

    /// Every AP write of `value`, as an index into the decoded transfer list.
    fn writes_of(xfers: &[Xfer], reg: u8, value: u32) -> Vec<usize> {
        xfers
            .iter()
            .enumerate()
            .filter(|(_, x)| !x.read && x.reg == reg && x.value == value)
            .map(|(i, _)| i)
            .collect()
    }

    /// The access at `drw_index` must be bracketed by a CSW selecting `size`, which is what makes it
    /// a 16-bit bus cycle rather than a 32-bit one over the same address.
    fn assert_width(xfers: &[Xfer], drw_index: usize, size: u32, what: &str) {
        let opened = xfers[..drw_index]
            .iter()
            .rev()
            .find(|x| !x.read && x.reg == 0x00)
            .unwrap_or_else(|| panic!("{what}: no CSW write precedes the access"));
        assert_eq!(opened.value, size, "{what}: the access ran at the wrong width");
    }

    #[test]
    fn same54_erase_block_drives_ctrlb() {
        let mut replies = Vec::new();
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(4));
        replies.extend(acks(2));
        replies.extend(acks(2));
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(2));
        replies.push(same54_ready_reply());
        replies.extend(acks(1));

        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.erase_flash_block(0x0000_2345).unwrap();
        let x = transfers(&target.inner().transport().sent);

        let cleared = writes_of(&x, 0x0c, u32::from(SAME54_COMMAND_ERRORS | SAME54_INTFLAG_DONE));
        assert_eq!(cleared.len(), 1, "the sticky flags are cleared exactly once");
        assert_width(&x, cleared[0], CSW_HALF, "clearing INTFLAG");

        let addr = writes_of(&x, 0x0c, 0x2000);
        assert_eq!(addr.len(), 1);
        assert_width(&x, addr[0], CSW_WORD, "ADDR");

        let command = writes_of(&x, 0x0c, 0x0000_a501);
        assert_eq!(command.len(), 1, "one erase command was issued");
        assert!(addr[0] < command[0], "ADDR is set before the command that acts on it");
    }

    /// An erase the controller REFUSED must not return `Ok`, and before the INTFLAG check it did.
    ///
    /// The mock answers `STATUS.READY` exactly as the passing case does -- because a controller that
    /// refused a command IS ready -- and differs only in the INTFLAG read. That is the whole defect
    /// in one reply: readiness could never have told these two apart.
    #[test]
    fn same54_erase_reports_a_refusal_instead_of_returning_ok() {
        let locked = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x08, 0x00, 0x00, 0x00];
        let mut replies = Vec::new();
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(4));
        replies.extend(acks(2));
        replies.extend(acks(2));
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(2));
        replies.push(locked);
        replies.extend(acks(1));

        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        match target.erase_flash_block(0x0000_2345) {
            Err(ProbeError::Device(reason)) => assert!(
                reason.contains("locked"),
                "the refusal must name the lock, got {reason:?}"
            ),
            other => panic!("a refused erase must not report success, got {other:?}"),
        }
    }

    #[test]
    fn same54_write_flash_clears_wmode_then_writes_page() {
        let ctrla_before = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x14, 0x00, 0x00, 0x00];
        let ctrla_after = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x04, 0x00, 0x00, 0x00];
        let mut replies = Vec::new();
        replies.extend(acks(2));
        replies.push(ctrla_before);
        replies.extend(acks(1));
        replies.extend(acks(4));
        replies.extend(acks(2));
        replies.push(ctrla_after);
        replies.extend(acks(1));
        replies.extend(acks(4));
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(2));
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(1));
        replies.push(block_ack(1));
        replies.extend(acks(2));
        replies.extend(acks(2));
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(2));
        replies.push(same54_ready_reply());
        replies.extend(acks(1));

        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        Same54Flash::write_flash(&mut target, 0x0, &[0xcafe_babe]).unwrap();
        let sent = target.inner().transport().sent.clone();
        let x = transfers(&sent);

        let wmode = writes_of(&x, 0x0c, 0x0000_0004);
        assert_eq!(wmode.len(), 1, "CTRLA is written once, with WMODE manual");
        assert_width(&x, wmode[0], CSW_HALF, "CTRLA");

        let ctrla_reads = x.iter().filter(|a| a.read && a.reg == 0x0c).count();
        assert!(ctrla_reads >= 2, "CTRLA is read back after the write, not assumed to have taken");

        let pbc = writes_of(&x, 0x0c, 0x0000_a515);
        let wp = writes_of(&x, 0x0c, 0x0000_a503);
        assert_eq!((pbc.len(), wp.len()), (1, 1), "one page: clear the buffer, then commit it");
        assert!(wmode[0] < pbc[0] && pbc[0] < wp[0], "manual mode, then clear, then commit");

        let block = sent.iter().find(|f| f[0] == proto::cmd::TRANSFER_BLOCK).expect("block write");
        assert_eq!(&block[5..9], &0xcafe_babeu32.to_le_bytes());
    }

    /// The controller has to CONFIRM manual mode. A part that ignores the `CTRLA` write -- because
    /// the access was refused, or because the Peripheral Access Controller protects the register --
    /// leaves `WMODE` where the resident application put it, and then filling the page buffer means
    /// something entirely different from what this driver intends.
    ///
    /// Nothing looked. The write went out, the commands after it all succeeded, and the flash ended
    /// up holding something nobody asked for with no error raised anywhere.
    #[test]
    fn same54_write_flash_refuses_when_wmode_does_not_take() {
        let stuck = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x14, 0x00, 0x00, 0x00];
        let mut replies = Vec::new();
        replies.extend(acks(2));
        replies.push(stuck.clone());
        replies.extend(acks(1));
        replies.extend(acks(4));
        replies.extend(acks(2));
        replies.push(stuck);
        replies.extend(acks(1));
        replies.extend(acks(4));
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(2));
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(1));
        replies.push(block_ack(1));
        replies.extend(acks(2));
        replies.extend(acks(2));
        replies.extend(acks(1));
        replies.push(same54_ready_reply());
        replies.extend(acks(2));
        replies.push(same54_ready_reply());
        replies.extend(acks(1));

        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        match Same54Flash::write_flash(&mut target, 0x0, &[0xcafe_babe]) {
            Err(ProbeError::Device(reason)) => assert!(
                reason.contains("WMODE"),
                "the refusal must name the mode that did not take, got {reason:?}"
            ),
            other => panic!("filling a page buffer in the wrong write mode must not proceed, got {other:?}"),
        }
        let x = transfers(&target.inner().transport().sent);
        assert!(
            writes_of(&x, 0x0c, 0x0000_a503).is_empty(),
            "no Write-Page command may be issued after the mode check fails"
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

    /// The SAM3X's chip id is at a different address from the SAM4's and names its member on the
    /// CIDR alone.
    ///
    /// **THE ADDRESS IS THE HALF THAT COSTS A BOARD.** A tool carrying the SAM4's `0x400E0740`
    /// across to a SAM3X reads a TWI controller and decides whether to erase flash on what it
    /// finds there, which is the same class of mistake as reading the DSU on a part that has none.
    #[test]
    fn the_sam3x_chip_id_is_its_own_address_and_its_own_table() {
        assert_eq!(SAM3X_CHIPID_CIDR, 0x400e_0940);
        assert_eq!(SAM3X_CHIPID_EXID, 0x400e_0944);
        assert_ne!(SAM3X_CHIPID_CIDR, SAM4_CHIPID_CIDR);

        assert_eq!(sam3x_identify(0x285e_0a60), Some("ATSAM3X8E (144-pin, 2 x 256 KB)"));
        assert_eq!(sam3x_identify(0x285e_0a61), Some("ATSAM3X8E (144-pin, 2 x 256 KB)"));
        assert_eq!(sam3x_identify(0x285e_0a7f), Some("ATSAM3X8E (144-pin, 2 x 256 KB)"));
        assert_eq!(sam3x_identify(0x285e_0a40), None);

        assert_eq!(sam3x_identify(0xa3cc_0ce0), None, "an ATSAM4E16E");
        assert_eq!(sam4_identify(0x285e_0a60, 0), None, "an ATSAM3X8E");
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
            ack.clone(),
            frr_word(262_144),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let descriptor = target.sam3x_flash_descriptor(SAM3X_EEFC0).unwrap();
        assert_eq!(
            descriptor,
            Sam3xFlashDescriptor {
                interface: 0x000f_0640,
                size: 262_144,
                page_size: 256,
                planes: 1,
                plane_bytes: 262_144,
            }
        );
        assert_eq!(descriptor.size, descriptor.plane_bytes);
        assert_eq!(descriptor.planes, 1);
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
    fn device_id_names_each_of_the_three_parts() {
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

    /// The SAM E51, read off a Curiosity Nano: `DID 0x61810604`.
    ///
    /// Two separate properties, and both are easy to lose.
    ///
    /// **Its SERIES is `0x1`, which is ALSO the SAM D21's**, so the two parts are told apart only
    /// by `PROCESSOR` and `FAMILY`. A predicate that grew lax about either would hand an E51 to the
    /// D21 routines, which drive a controller whose command register sits where the E51 keeps its
    /// configuration register. The E54 case cannot catch that: its series does not collide.
    ///
    /// **The collision is the point, and it outlives the claim question.** Series `0x1` names a
    /// SAM D21 in one family and a SAM E51 in another, so `series` alone decides nothing here --
    /// only the (processor, family, series) triple does. That half of this test never changed.
    ///
    /// The E51's CLAIM is the other half, and it is asserted here rather than described: see
    /// [`SamDeviceId::drives_same54_nvmctrl`] for what that claim rests on and for the caveat that
    /// travels with it.
    #[test]
    fn the_e51_shares_a_series_number_with_the_d21_and_only_the_triple_tells_them_apart() {
        let e51 = SamDeviceId::decode(0x6181_0604);
        assert_eq!((e51.processor, e51.family, e51.series), (0x6, 0x3, 0x1));
        assert_eq!(e51.revision_letter(), 'G');
        assert_eq!(e51.part(), Some("ATSAME51J20A"));

        let d21 = SamDeviceId::decode(0x1001_0000);
        assert_eq!(e51.series, d21.series, "the collision this test is about");
        assert!(!e51.drives_samd21_nvmctrl(), "an E51 driven as a D21 writes the wrong registers");
        assert!(d21.drives_samd21_nvmctrl());
        assert!(!d21.drives_same54_nvmctrl(), "and the collision must not run the other way either");

        assert!(
            e51.drives_same54_nvmctrl(),
            "an E51 is driven by the E54 routine, which is verified against an E54 control across \
             both banks"
        );
        assert_eq!(e51.flash_routine(), Some("Same54Flash"));
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

    const FAKE_NVMCTRL: u32 = 0x4100_4000;
    const FAKE_CTRLA: u32 = FAKE_NVMCTRL;
    const FAKE_CTRLB: u32 = FAKE_NVMCTRL + 0x04;
    const FAKE_INTFLAG: u32 = FAKE_NVMCTRL + 0x14;
    const FAKE_STATUS: u32 = FAKE_NVMCTRL + 0x18;
    const FAKE_ADDR: u32 = FAKE_NVMCTRL + 0x1c;
    const FAKE_LOCK: u32 = FAKE_NVMCTRL + 0x20;
    const FAKE_LOCK_WORD: u32 = 0x5a5a_7ffe;
    const FAKE_ADDR_BITS: u32 = 0x003f_ffff;
    const FAKE_KEY: u32 = 0xa5 << 8;
    const FAKE_ER: u32 = 0x02;
    const FAKE_WP: u32 = 0x04;
    const FAKE_EAR: u32 = 0x05;
    const FAKE_WAP: u32 = 0x06;
    const FAKE_PBC: u32 = 0x44;
    const FAKE_MANW: u32 = 1 << 7;
    const FAKE_PROGE: u32 = 1 << 2;
    const FAKE_LOCKE: u32 = 1 << 3;
    const FAKE_NVME: u32 = 1 << 4;
    const FAKE_SB: u32 = 1 << 8;
    const FAKE_USER_ROW: u32 = 0x0080_4000;
    const FAKE_USER_ROW_IN_ADDR: u32 = 0x4000;
    const FAKE_MAIN: usize = 32 * 1024;
    const ZERO_ROW_WORD0: u32 = 0xd8e0_c7ff;
    const ZERO_ROW_WORD1: u32 = 0xffff_fc5d;

    /// A SAM D21 as its flash controller, 32 KB of main array, its NVM User Row, and the page buffer
    /// the two share.
    struct FakeSamd21 {
        log: Vec<String>,
        main: Vec<u8>,
        user_row: [u8; 256],
        /// The page buffer. A fill ANDs into it, and only a Page Buffer Clear or a page write sets
        /// it back to ones -- stricter than overwriting, so a missing clear cannot pass.
        buffer: [u8; 64],
        ctrlb: u32,
        addr: u32,
        status: u32,
        halted: bool,
        busy: bool,
        /// How many Write Auxiliary Page commands report NVME and program nothing, from the first.
        failing_writes: usize,
    }

    impl FakeSamd21 {
        /// A halted part with a patterned main array and the user row `row`.
        fn holding(row: &Samd21UserRow) -> Self {
            let mut user_row = [0u8; 256];
            user_row.copy_from_slice(&row.to_bytes());
            FakeSamd21 {
                log: Vec::new(),
                main: (0..FAKE_MAIN).map(|at| (at * 7 + 3) as u8).collect(),
                user_row,
                buffer: [0xff; 64],
                ctrlb: 0,
                addr: 0,
                status: 0,
                halted: true,
                busy: false,
                failing_writes: 0,
            }
        }

        fn row(&self) -> Samd21UserRow {
            Samd21UserRow::from_bytes(&self.user_row).unwrap()
        }

        fn in_user_row(address: u32) -> bool {
            (FAKE_USER_ROW..FAKE_USER_ROW + 256).contains(&address)
        }

        fn command(&mut self, value: u32) {
            if value & 0xff00 != FAKE_KEY {
                self.status |= FAKE_PROGE;
                self.log.push("bad key".to_owned());
                return;
            }
            let byte = (self.addr & FAKE_ADDR_BITS) << 1;
            match value & 0x7f {
                FAKE_PBC => {
                    self.buffer = [0xff; 64];
                    self.log.push("PBC".to_owned());
                }
                FAKE_EAR if self.status & FAKE_SB != 0 => self.status |= FAKE_PROGE,
                FAKE_EAR if byte & !0xff == FAKE_USER_ROW_IN_ADDR => {
                    self.user_row = [0xff; 256];
                    self.log.push("EAR".to_owned());
                }
                FAKE_WAP if self.status & FAKE_SB != 0 => self.status |= FAKE_PROGE,
                FAKE_WAP if byte & !0xff == FAKE_USER_ROW_IN_ADDR => {
                    let page = ((byte - FAKE_USER_ROW_IN_ADDR) / 64) as usize;
                    if self.failing_writes > 0 {
                        self.failing_writes -= 1;
                        self.status |= FAKE_NVME;
                        self.log.push(format!("WAP {page} failed"));
                        return;
                    }
                    for (cell, fill) in self.user_row[page * 64..page * 64 + 64]
                        .iter_mut()
                        .zip(self.buffer.iter())
                    {
                        *cell &= *fill;
                    }
                    self.buffer = [0xff; 64];
                    self.log.push(format!("WAP {page}"));
                }
                FAKE_ER => {
                    let row = (byte & !0xff) as usize;
                    if let Some(cells) = self.main.get_mut(row..row + 256) {
                        cells.fill(0xff);
                    }
                    self.log.push(format!("ER {row:#x}"));
                }
                FAKE_WP => self.log.push(format!("WP {byte:#x}")),
                other => {
                    self.status |= FAKE_PROGE;
                    self.log.push(format!("refused command {other:#x}"));
                }
            }
        }

        /// A bus write into the NVM address space, which loads the page buffer.
        fn fill(&mut self, address: u32, value: u32) {
            if self.ctrlb & FAKE_MANW == 0 {
                self.status |= FAKE_PROGE;
                self.log.push("fill with automatic writes on".to_owned());
                return;
            }
            let at = (address % 64) as usize;
            for (cell, byte) in self.buffer[at..at + 4].iter_mut().zip(value.to_le_bytes()) {
                *cell &= byte;
            }
            self.addr = (address >> 1) & FAKE_ADDR_BITS;
        }
    }

    impl TargetAccess for FakeSamd21 {
        fn connect(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            Ok(0x0bc1_1477)
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            let word = |bytes: &[u8], at: usize| {
                u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            };
            match address {
                FAKE_INTFLAG => Ok(u32::from(!self.busy)),
                FAKE_STATUS => Ok(self.status),
                FAKE_CTRLB => Ok(self.ctrlb),
                FAKE_ADDR => Ok(self.addr),
                FAKE_LOCK => Ok(FAKE_LOCK_WORD),
                0x0080_a00c => Ok(0x5e71_0000),
                0x0080_a040 => Ok(0x5e71_0001),
                0x0080_a044 => Ok(0x5e71_0002),
                0x0080_a048 => Ok(0x5e71_0003),
                _ if Self::in_user_row(address) => {
                    Ok(word(&self.user_row, (address - FAKE_USER_ROW) as usize))
                }
                _ if (address as usize) < FAKE_MAIN => Ok(word(&self.main, address as usize)),
                _ => Err(ProbeError::Device("a read of an address this fake does not model")),
            }
        }
        fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
            match address {
                FAKE_CTRLA => self.command(value),
                FAKE_CTRLB => self.ctrlb = value,
                FAKE_STATUS => self.status &= !(value & (FAKE_PROGE | FAKE_LOCKE | FAKE_NVME)),
                FAKE_ADDR => self.addr = value & FAKE_ADDR_BITS,
                _ if Self::in_user_row(address) || (address as usize) < FAKE_MAIN => {
                    self.fill(address, value)
                }
                _ => return Err(ProbeError::Device("a write to an address this fake does not model")),
            }
            Ok(())
        }
        fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
            for (index, word) in out.iter_mut().enumerate() {
                *word = self.read_word(address + 4 * index as u32)?;
            }
            Ok(())
        }
        fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
            for (index, word) in words.iter().enumerate() {
                self.write_word(address + 4 * index as u32, *word)?;
            }
            Ok(())
        }
        fn read_byte(&mut self, _: u32) -> Result<u8, ProbeError> {
            Err(ProbeError::Device("byte access is not modelled"))
        }
        fn write_byte(&mut self, _: u32, _: u8) -> Result<(), ProbeError> {
            Err(ProbeError::Device("byte access is not modelled"))
        }
        fn read_halfword(&mut self, _: u32) -> Result<u16, ProbeError> {
            Err(ProbeError::Device("half-word access is not modelled"))
        }
        fn write_halfword(&mut self, _: u32, _: u16) -> Result<(), ProbeError> {
            Err(ProbeError::Device("half-word access is not modelled"))
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            self.halted = true;
            Ok(())
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            self.halted = false;
            Ok(())
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            Ok(self.halted)
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("reset is not modelled"))
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("reset is not modelled"))
        }
        fn set_reset(&mut self, _: bool) -> Result<u8, ProbeError> {
            Err(ProbeError::Device("reset is not modelled"))
        }
        fn read_core_reg(&mut self, _: u8) -> Result<u32, ProbeError> {
            Err(ProbeError::Device("core registers are not modelled"))
        }
        fn write_core_reg(&mut self, _: u8, _: u32) -> Result<(), ProbeError> {
            Err(ProbeError::Device("core registers are not modelled"))
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("reset is not modelled"))
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("reset is not modelled"))
        }
        fn set_breakpoint(&mut self, _: u32) -> Result<(), ProbeError> {
            Err(ProbeError::Device("breakpoints are not modelled"))
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("breakpoints are not modelled"))
        }
        fn set_breakpoints(&mut self, _: &[u32]) -> Result<(), ProbeError> {
            Err(ProbeError::Device("breakpoints are not modelled"))
        }
        fn call_target(
            &mut self,
            _: u32,
            _: &[u32],
            _: &lamella_probe_core::CallFrame,
        ) -> Result<u32, ProbeError> {
            Err(ProbeError::Device("calls are not modelled"))
        }
    }

    /// The Zero's row as read, and the same row with an 8 KB bootloader protected -- what a board
    /// whose bootloader was burned with protection set would read.
    fn zero_row() -> Samd21UserRow {
        let mut words = [u32::MAX; SAMD21_USER_ROW_WORDS];
        words[0] = ZERO_ROW_WORD0;
        words[1] = ZERO_ROW_WORD1;
        Samd21UserRow { words }
    }

    fn protected_row() -> Samd21UserRow {
        let mut row = zero_row();
        row.words[0] = (row.words[0] & !0b111) | 0x2;
        row
    }

    /// The user row's commands and registers, pinned against DS40001882D. EAR and WAP sit next to
    /// ER and WP in the command table, and each pair takes the same ADDR -- so one wrong digit
    /// erases a main-array row instead.
    #[test]
    fn the_user_row_constants_are_the_datasheets() {
        assert_eq!([SAMD21_CMD_EAR, SAMD21_CMD_WAP], [FAKE_EAR, FAKE_WAP]);
        assert_ne!(SAMD21_CMD_EAR, SAMD21_CMD_ER);
        assert_ne!(SAMD21_CMD_WAP, SAMD21_CMD_WP);
        assert_eq!(SAMD21_STATUS, FAKE_STATUS);
        assert_eq!(
            [SAMD21_STATUS_PROGE, SAMD21_STATUS_LOCKE, SAMD21_STATUS_NVME, SAMD21_STATUS_SB],
            [FAKE_PROGE, FAKE_LOCKE, FAKE_NVME, FAKE_SB]
        );
        assert_eq!(SAMD21_USER_ROW, FAKE_USER_ROW);
        assert_eq!(SAMD21_USER_ROW / 2, 0x0040_2000);
        assert_eq!((SAMD21_USER_ROW / 2) & FAKE_ADDR_BITS, FAKE_USER_ROW_IN_ADDR / 2);
        assert_eq!(SAMD21_USER_ROW_WORDS * 4, 256);
    }

    /// The user-row layout is claimed for the SAM D21's series alone, and the Zero's own DID is in it.
    #[test]
    fn only_a_sam_d21_claims_this_user_row() {
        assert!(SamDeviceId::decode(0x1001_0305).has_samd21_user_row(), "the Zero's DID");
        assert!(SamDeviceId::decode(0x1001_0300).has_samd21_user_row(), "an ATSAMD21J18A");
        for other in [0x1003_0000u32, 0x1002_0000, 0x6184_0300, 0x6181_0604] {
            let id = SamDeviceId::decode(other);
            assert!(!id.has_samd21_user_row(), "{other:#010x}");
        }
        assert!(SamDeviceId::decode(0x1003_0000).drives_samd21_nvmctrl());
    }

    /// The serial number is four words at four addresses, and word 0 sits apart from the other
    /// three -- so a read that walked one block from word 0 would return three wrong words.
    #[test]
    fn the_serial_number_is_read_word_by_word_in_the_datasheets_order() {
        let mut part = FakeSamd21::holding(&zero_row());
        assert_eq!(
            part.read_samd21_serial_number().unwrap(),
            [0x5e71_0000, 0x5e71_0001, 0x5e71_0002, 0x5e71_0003]
        );
        assert!(part.log.is_empty(), "a read issues no command");
    }

    #[test]
    fn the_user_row_fields_cover_each_bit_once() {
        let mut covered = 0u64;
        let mut next = 0;
        for field in SAMD21_USER_ROW_FIELDS {
            assert_eq!(field.low_bit, next, "{} starts where the one before it ends", field.name);
            let bits = ((1u64 << field.width) - 1) << field.low_bit;
            assert_eq!(covered & bits, 0, "{} overlaps a field before it", field.name);
            covered |= bits;
            next = field.low_bit + field.width;
        }
        assert_eq!(covered, u64::MAX, "the fields cover all 64 bits");
    }

    /// The Zero's row, decoded field by field. Every expected value is read by hand from Table
    /// 10-4, and the BOD12 values are the table's production defaults.
    #[test]
    fn the_zeros_row_decodes_as_table_10_4_lays_it_out() {
        let row = zero_row();
        let decoded: Vec<(&str, u32)> = SAMD21_USER_ROW_FIELDS
            .iter()
            .map(|field| (field.name, row.field(field)))
            .collect();
        assert_eq!(
            decoded,
            [
                ("BOOTPROT", 7),
                ("reserved", 1),
                ("EEPROM", 7),
                ("reserved", 1),
                ("BOD33 level", 0x07),
                ("BOD33 enable", 1),
                ("BOD33 action", 1),
                ("BOD12 configuration", 0x70),
                ("WDT enable", 0),
                ("WDT always-on", 0),
                ("WDT period", 0xb),
                ("WDT window", 0xb),
                ("WDT early-warning offset", 0xb),
                ("WDT window mode", 0),
                ("BOD33 hysteresis", 0),
                ("BOD12 configuration", 0),
                ("reserved", 0x3f),
                ("LOCK", 0xffff),
            ]
        );
        assert_eq!(row.bootprot(), SAMD21_BOOTPROT_NONE);
    }

    #[test]
    fn bootprot_protects_what_table_22_2_says() {
        let bytes: Vec<u32> = (0..=7).map(samd21_bootprot_bytes).collect();
        assert_eq!(bytes, [32_768, 16_384, 8_192, 4_096, 2_048, 1_024, 512, 0]);
        assert_eq!(samd21_bootprot_bytes(protected_row().bootprot()), 0x2000);
    }

    #[test]
    fn eeprom_reserves_what_table_22_3_says() {
        let bytes: Vec<u32> = (0..=7).map(samd21_eeprom_bytes).collect();
        assert_eq!(bytes, [16_384, 8_192, 4_096, 2_048, 1_024, 512, 256, 0]);
        assert_eq!(zero_row().eeprom(), SAMD21_EEPROM_NONE);
    }

    /// The EEPROM and LOCK accessors read the bits Table 10-4 gives those fields, on rows whose
    /// neighboring bits are ones, zeros, and a pattern.
    #[test]
    fn the_eeprom_and_lock_fields_are_where_table_10_4_puts_them() {
        let field = |name: &str| {
            *SAMD21_USER_ROW_FIELDS.iter().find(|field| field.name == name).expect("a field")
        };
        let mut patterned = zero_row();
        for (index, word) in patterned.words.iter_mut().enumerate() {
            *word = 0x9e37_79b9u32.rotate_left(index as u32);
        }
        let rows = [
            zero_row(),
            Samd21UserRow { words: [0; SAMD21_USER_ROW_WORDS] },
            Samd21UserRow { words: [u32::MAX; SAMD21_USER_ROW_WORDS] },
            patterned,
        ];
        for row in rows {
            assert_eq!(u32::from(row.eeprom()), row.field(&field("EEPROM")), "{row:08x?}");
            assert_eq!(u32::from(row.lock()), row.field(&field("LOCK")), "{row:08x?}");
        }
    }

    /// NVMCTRL.LOCK is the low half of the word at offset 0x20; the reserved half above it is not
    /// part of the reading.
    #[test]
    fn the_region_locks_are_the_low_half_of_nvmctrl_lock() {
        let mut part = FakeSamd21::holding(&zero_row());
        assert_eq!(part.read_samd21_region_locks().expect("a read"), 0x7ffe);
        assert!(part.log.is_empty(), "a read issues no command: {:?}", part.log);
    }

    /// A BOOTPROT change touches bits 2:0 of the first word and nothing else, for every value and
    /// for rows whose neighboring bits are ones, zeros, and a pattern.
    #[test]
    fn a_bootprot_change_keeps_every_other_bit() {
        let mut patterned = zero_row();
        for (index, word) in patterned.words.iter_mut().enumerate() {
            *word = 0x9e37_79b9u32.rotate_left(index as u32);
        }
        let rows = [
            zero_row(),
            protected_row(),
            Samd21UserRow { words: [0; SAMD21_USER_ROW_WORDS] },
            Samd21UserRow { words: [u32::MAX; SAMD21_USER_ROW_WORDS] },
            patterned,
        ];
        for row in rows {
            for bootprot in 0..=7u8 {
                let changed = row.with_bootprot(bootprot).unwrap();
                assert_eq!(changed.bootprot(), bootprot);
                assert_eq!(changed.words[0] & !0b111, row.words[0] & !0b111, "word 0 outside BOOTPROT");
                assert_eq!(changed.words[1..], row.words[1..], "every later word");
            }
            assert_eq!(row.with_bootprot(8), None, "a value wider than the field");
        }
    }

    #[test]
    fn a_backup_holds_the_row_in_address_order() {
        let row = zero_row();
        let bytes = row.to_bytes();
        assert_eq!(bytes.len(), 256);
        assert_eq!(bytes[..8], [0xff, 0xc7, 0xe0, 0xd8, 0x5d, 0xfc, 0xff, 0xff]);
        assert_eq!(Samd21UserRow::from_bytes(&bytes), Some(row));
        assert_eq!(Samd21UserRow::from_bytes(&bytes[..255]), None);
    }

    /// The rewrite a protected board takes: the user row erased by its own command, the one page
    /// that holds data programmed by its own command, the main array untouched, and the controller's
    /// settings as they were found.
    #[test]
    fn a_rewrite_uses_the_user_rows_own_commands_and_nothing_else() {
        let before = protected_row();
        let mut part = FakeSamd21::holding(&before);
        let main = part.main.clone();
        let after = part.rewrite_samd21_bootprot(&before, SAMD21_BOOTPROT_NONE).unwrap();
        assert_eq!(after, zero_row(), "BOOTPROT 7 and every other bit as read");
        assert_eq!(part.row(), zero_row(), "and the part holds that row");
        assert_eq!(part.log, ["EAR", "PBC", "WAP 0"], "pages 1 to 3 are erased and stay so");
        assert!(part.main == main, "the main array is untouched");
        assert_eq!(part.ctrlb, 0, "CTRLB goes back as it was found");
        assert_eq!(part.status, 0, "no error left behind");
    }

    #[test]
    fn a_rewrite_programs_every_page_that_holds_data() {
        let mut before = protected_row();
        before.words[16 * 2 + 3] = 0x1234_5678;
        let mut part = FakeSamd21::holding(&before);
        part.rewrite_samd21_bootprot(&before, SAMD21_BOOTPROT_NONE).unwrap();
        assert_eq!(part.log, ["EAR", "PBC", "WAP 0", "PBC", "WAP 2"]);
        assert_eq!(part.row(), before.with_bootprot(SAMD21_BOOTPROT_NONE).unwrap());
    }

    #[test]
    fn a_rewrite_to_the_value_already_held_writes_nothing() {
        let row = zero_row();
        let mut part = FakeSamd21::holding(&row);
        assert_eq!(part.rewrite_samd21_bootprot(&row, SAMD21_BOOTPROT_NONE), Ok(row));
        assert!(part.log.is_empty(), "no command: {:?}", part.log);
    }

    /// Every refusal happens before anything is erased, and says so.
    #[test]
    fn a_rewrite_refuses_before_erasing_anything() {
        type Arrange = fn(&mut FakeSamd21);
        let row = protected_row();
        let refusals: [(&str, Arrange); 4] = [
            ("running", |part| part.halted = false),
            ("security bit", |part| part.status |= FAKE_SB),
            ("busy", |part| part.busy = true),
            ("no longer reads", |part| part.user_row[40] = 0),
        ];
        for (expected, arrange) in refusals {
            let mut part = FakeSamd21::holding(&row);
            arrange(&mut part);
            let held = part.user_row;
            match part.rewrite_samd21_bootprot(&row, SAMD21_BOOTPROT_NONE) {
                Err(Samd21UserRowError::Unchanged(why)) => {
                    assert!(why.contains(expected), "{expected:?} in {why:?}")
                }
                other => panic!("{expected}: {other:?}"),
            }
            assert!(part.log.is_empty(), "{expected}: no command ran, but {:?}", part.log);
            assert_eq!(part.user_row, held, "{expected}: the row is as it was");
        }
        let mut part = FakeSamd21::holding(&row);
        assert!(matches!(
            part.rewrite_samd21_bootprot(&row, 8),
            Err(Samd21UserRowError::Unchanged(_))
        ));
    }

    /// A page write the controller reports as failed is repeated from the erase, and the second
    /// attempt finishes the row.
    #[test]
    fn a_failed_page_write_is_repeated_from_the_erase() {
        let before = protected_row();
        let mut part = FakeSamd21::holding(&before);
        part.failing_writes = 1;
        let after = part.rewrite_samd21_bootprot(&before, SAMD21_BOOTPROT_NONE).unwrap();
        assert_eq!(after, zero_row());
        assert_eq!(part.log, ["EAR", "PBC", "WAP 0 failed", "EAR", "PBC", "WAP 0"]);
        assert_eq!(part.status, 0, "the failure's NVME is cleared, not left behind");
    }

    /// When no attempt finishes, the error says the row was erased and carries what it holds now.
    #[test]
    fn a_rewrite_that_cannot_finish_reports_the_row_it_left() {
        let before = protected_row();
        let mut part = FakeSamd21::holding(&before);
        part.failing_writes = usize::MAX;
        match part.rewrite_samd21_bootprot(&before, SAMD21_BOOTPROT_NONE) {
            Err(Samd21UserRowError::NotRewritten { reason, now }) => {
                assert!(reason.contains("NVME"), "{reason}");
                assert_eq!(now.as_deref(), Some(&Samd21UserRow { words: [u32::MAX; SAMD21_USER_ROW_WORDS] }));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(part.log.iter().filter(|line| *line == "EAR").count(), SAMD21_USER_ROW_ATTEMPTS);
        assert_eq!(part.ctrlb, 0, "CTRLB goes back as it was found on this path too");
    }

    /// What a rewrite can leave on the way to a saved copy, and what it cannot.
    #[test]
    fn a_row_is_partway_to_its_saved_copy_only_by_programming() {
        let saved = protected_row();
        let erased = Samd21UserRow { words: [u32::MAX; SAMD21_USER_ROW_WORDS] };
        assert!(erased.is_partway_to(&saved), "erased");
        assert!(zero_row().is_partway_to(&saved), "BOOTPROT alone differs");
        assert!(saved.is_partway_to(&saved), "the copy itself");
        let mut half = erased;
        half.words[0] = zero_row().words[0];
        assert!(half.is_partway_to(&saved), "part-way");
        let mut other = zero_row();
        other.words[0] &= !(1 << 21);
        assert!(!other.is_partway_to(&saved), "a production bit cleared");
    }

    /// The way back from a rewrite that could not finish: the row left erased, then the saved copy
    /// put back by the user row's own commands, with the main array untouched.
    #[test]
    fn a_restore_puts_the_saved_copy_back_over_an_unfinished_rewrite() {
        let saved = protected_row();
        let mut part = FakeSamd21::holding(&saved);
        part.failing_writes = usize::MAX;
        let left = match part.rewrite_samd21_bootprot(&saved, SAMD21_BOOTPROT_NONE) {
            Err(Samd21UserRowError::NotRewritten { now: Some(row), .. }) => *row,
            other => panic!("{other:?}"),
        };
        part.failing_writes = 0;
        part.log.clear();
        let main = part.main.clone();
        assert_eq!(part.restore_samd21_user_row(&left, &saved), Ok(saved));
        assert_eq!(part.row(), saved);
        assert_eq!(part.log, ["EAR", "PBC", "WAP 0"]);
        assert!(part.main == main, "the main array is untouched");
        assert_eq!(part.ctrlb, 0);
    }

    #[test]
    fn a_restore_undoes_a_finished_rewrite() {
        let saved = protected_row();
        let mut part = FakeSamd21::holding(&saved);
        let cleared = part.rewrite_samd21_bootprot(&saved, SAMD21_BOOTPROT_NONE).unwrap();
        part.log.clear();
        assert_eq!(part.restore_samd21_user_row(&cleared, &saved), Ok(saved));
        assert_eq!(part.row().bootprot(), 2);
        assert_eq!(part.log, ["EAR", "PBC", "WAP 0"]);
    }

    #[test]
    fn a_restore_of_the_row_already_held_writes_nothing() {
        let row = zero_row();
        let mut part = FakeSamd21::holding(&row);
        assert_eq!(part.restore_samd21_user_row(&row, &row), Ok(row));
        assert!(part.log.is_empty(), "{:?}", part.log);
    }

    /// A saved copy the row cannot have come from is refused before anything is erased: its
    /// production-written bits are not this row's.
    #[test]
    fn a_restore_refuses_a_copy_that_is_not_this_rows_past() {
        let now = zero_row();
        let mut saved = now;
        saved.words[0] |= 1 << 17;
        let mut part = FakeSamd21::holding(&now);
        match part.restore_samd21_user_row(&now, &saved) {
            Err(Samd21UserRowError::Unchanged(why)) => {
                assert!(why.contains("not this row's past"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        assert!(part.log.is_empty(), "{:?}", part.log);
        assert_eq!(part.row(), now);
    }

    #[test]
    fn a_restore_makes_the_rewrites_checks_before_erasing() {
        let saved = protected_row();
        let mut part = FakeSamd21::holding(&zero_row());
        part.halted = false;
        assert!(matches!(
            part.restore_samd21_user_row(&zero_row(), &saved),
            Err(Samd21UserRowError::Unchanged(_))
        ));
        assert!(part.log.is_empty(), "running: {:?}", part.log);
        let mut part = FakeSamd21::holding(&zero_row());
        assert!(matches!(
            part.restore_samd21_user_row(&saved, &saved),
            Err(Samd21UserRowError::Unchanged(_))
        ));
        assert!(part.log.is_empty(), "the row changed since it was shown: {:?}", part.log);
    }
}
