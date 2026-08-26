//! The flashing backends this lane owns, implemented against the contract.

use lamella_cmsis_dap_nrf::Nrf51Flash;
use lamella_cmsis_dap_stm32::{
    STM32L0_FLASH_BASE, STM32L0_FLASH_SIZE_REG, STM32L0_PAGE, Stm32L0Category, Stm32L0Flash,
    stm32_flash_size_bytes, stm32l0_dev_id,
};
use lamella_flash_backend::{FlashBackend, FlashError, Image, PartIdentity};
use lamella_probe_core::{TargetAccess, TargetAccessExt};

/// A micro:bit's on-board DAPLink probe, over SWD.
///
/// Generic over the target rather than owning a probe, for the reason
/// `lamella-cmsis-dap-nrf`'s own header gives about its trait: the routines drive an nRF through
/// whatever probe reached it, so nothing here needs to change when a probe family is added -- and a
/// test can drive it with a fake target and no hardware at all.
pub struct MicrobitDaplink<A: TargetAccess> {
    target: A,
    expect: PartIdentity,
}

impl<A: TargetAccess> MicrobitDaplink<A> {
    /// A backend for a part whose debug port answers `idcode`.
    ///
    /// `what` says what that reading actually settles, and it is not "which board this is": the
    /// nRF51's `0x0bb11477` is the generic Cortex-M0 SW-DP id and an STM32F0 answers the same. It
    /// separates a v1 from a v2 -- which is the confusion that erases a board -- and nothing finer.
    pub fn new(target: A, idcode: u32, what: &'static str) -> Self {
        MicrobitDaplink { target, expect: PartIdentity { value: u64::from(idcode), what } }
    }
}

impl<A: TargetAccess> FlashBackend for MicrobitDaplink<A> {
    fn mechanism(&self) -> &'static str {
        "the board's on-board DAPLink probe, over SWD"
    }

    fn flash_base(&self) -> u32 {
        0
    }

    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        self.target.connect()?;
        let found = u64::from(self.target.read_idcode()?);
        if found != self.expect.value {
            return Err(FlashError::WrongPart { expected: self.expect.clone(), found });
        }
        Ok(self.expect.clone())
    }

    fn erase(&mut self, _image: &Image<'_>) -> Result<(), FlashError> {
        self.target.init_mem()?;
        self.target.halt()?;
        self.target.erase_all()?;
        Ok(())
    }

    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let words = to_words(image.bytes);
        self.target.write_flash(image.base, &words)?;
        Ok(())
    }

    fn read_back(&mut self, image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>> {
        Some(self.read_span(image))
    }

    fn finish(&mut self) -> Result<(), FlashError> {
        self.target.reset_and_run()?;
        Ok(())
    }
}

impl<A: TargetAccess> MicrobitDaplink<A> {
    /// Read back exactly the bytes `image` covers.
    ///
    /// Reads WORDS because that is what the memory interface offers, then trims to the image's
    /// length: a four-byte-aligned read of a span that is not a multiple of four would otherwise
    /// report more bytes than were written, which the contract's comparison correctly refuses as a
    /// short-read's mirror image.
    fn read_span(&mut self, image: &Image<'_>) -> Result<Vec<u8>, FlashError> {
        let words = image.bytes.len().div_ceil(4);
        let read = self.target.read_words(image.base, words)?;
        let mut bytes = Vec::with_capacity(words * 4);
        for word in read {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.truncate(image.bytes.len());
        Ok(bytes)
    }
}

/// An image as little-endian 32-bit words, zero-padding a trailing partial word.
///
/// The padding matches what the part crate's own orchestrator does, so a program whose length is
/// not a multiple of four is written identically either way -- and the read-back is trimmed to the
/// image's real length rather than compared against the padding.
fn to_words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks(4)
        .map(|chunk| {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            u32::from_le_bytes(word)
        })
        .collect()
}


/// An RP2350 over an SWD probe, programmed by the chip's own bootrom flash API.
///
/// **THE ALTERNATIVE TO THE BOOTLOADER VOLUME, AND THE ONLY ROUTE TO THIS PART THAT CAN VERIFY.**
/// A volume write cannot be read back at all; this reads every byte through the XIP window and
/// compares it.
///
/// **THIS BACKEND IS COARSER THAN THE MICRO:BIT ONE, because the part crate's `flash_image` is an
/// orchestrator.** It performs reset-halt, secure setup, erase, program, its own verify and a reset
/// in a single call and does not expose those steps separately, so [`erase`](FlashBackend::erase)
/// here is empty and the erase happens inside [`program`](FlashBackend::program). The contract's
/// ORDER still holds: [`identify`](FlashBackend::identify) runs before `program` is ever called,
/// which is the guarantee that matters.
pub struct Rp2350Probe<A: TargetAccess> {
    target: A,
    expect: PartIdentity,
}

impl<A: TargetAccess> Rp2350Probe<A> {
    /// A backend for a part whose debug port answered `idcode`.
    ///
    /// The connect happens before construction because the part crate's `connect` is concrete over
    /// its transport rather than generic over [`TargetAccess`] -- so the reading is taken first and
    /// checked here, which keeps identify-before-erase true even though the read did not happen
    /// inside this type.
    pub fn new(target: A, idcode: u32, what: &'static str) -> Self {
        Rp2350Probe { target, expect: PartIdentity { value: u64::from(idcode), what } }
    }
}

/// `OTP_DATA_BASE`, the ECC-corrected alias of the OTP array (RP2350 datasheet 13).
///
/// Rows 0x000..0x003 hold the 64-bit chip id, one 32-bit read per two rows.
const OTP_CHIPID_BASE: u32 = 0x4013_0000;

impl<A: TargetAccess> FlashBackend for Rp2350Probe<A> {
    fn mechanism(&self) -> &'static str {
        "an SWD probe, by the chip's own bootrom flash API"
    }

    fn flash_base(&self) -> u32 {
        lamella_cmsis_dap_rp2350::XIP_BASE
    }

    /// The board's 64-bit OTP chip id -- the fact that identifies THIS board and not its family.
    ///
    /// **THE DEBUG-PORT ID IS NOT AN IDENTITY HERE.** `0x4c013477` is answered by every RP2350: a
    /// Pico 2, a Pico 2 W and a Pimoroni Pico Plus 2 are indistinguishable by it. On a bench
    /// holding several, reporting it as "the part" would name something true of all of them while
    /// the caller was deciding whether it may write ONE.
    ///
    /// The chip id is in OTP rows 0x000..0x003, read through the ECC alias at `OTP_DATA_BASE`
    /// (datasheet 13). It reads WITHOUT halting the core, so identifying costs the board nothing
    /// and can happen before anything is erased -- which is the order the contract requires
    /// anyway. It is also the value the bootloader publishes as its USB serial, so a board named
    /// from a BOOTSEL listing and a board identified here are named the same way.
    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        let low = self.target.read_word(OTP_CHIPID_BASE)?;
        let high = self.target.read_word(OTP_CHIPID_BASE + 4)?;
        let chip_id = (u64::from(high) << 32) | u64::from(low);
        Ok(PartIdentity { value: chip_id, what: self.expect.what })
    }

    /// Nothing: the erase happens inside [`program`](Self::program).
    ///
    /// See the type's own note. This is empty because the part crate does not offer an erase that
    /// can be called on its own, not because an RP2350 needs no erasing.
    fn erase(&mut self, _image: &Image<'_>) -> Result<(), FlashError> {
        Ok(())
    }

    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        lamella_cmsis_dap_rp2350::flash_image(&mut self.target, image.bytes, |line| {
            println!("  {line}");
        })?;
        Ok(())
    }

    /// Every byte, read back through the XIP window.
    ///
    /// **THIS IS A SECOND, INDEPENDENT CHECK.** `flash_image` verifies internally as it programs;
    /// this reads the flash again afterwards and lets the contract do the comparison. The two are
    /// deliberately not folded together -- a verify inside the routine that did the writing shares
    /// every assumption the writing made, and the point of the outer one is that it does not.
    ///
    /// The core is halted first: `flash_image` resets the chip to run the new image, and reading
    /// flash out from under a running program is a race the reader would lose silently.
    /// [`finish`](Self::finish) starts it again.
    fn read_back(&mut self, image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>> {
        Some(self.read_span(image))
    }

    fn finish(&mut self) -> Result<(), FlashError> {
        self.target.reset_and_run()?;
        Ok(())
    }
}

impl<A: TargetAccess> Rp2350Probe<A> {
    /// Halt, then read back exactly the bytes `image` covers.
    fn read_span(&mut self, image: &Image<'_>) -> Result<Vec<u8>, FlashError> {
        self.target.halt()?;
        let words = image.bytes.len().div_ceil(4);
        let read = self.target.read_words(image.base, words)?;
        let mut bytes = Vec::with_capacity(words * 4);
        for word in read {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.truncate(image.bytes.len());
        Ok(bytes)
    }
}

/// A board's bootloader mass-storage volume, which takes a UF2 file and nothing else.
///
/// **THIS MECHANISM CANNOT READ THE FLASH AND CANNOT IDENTIFY THE BOARD BEHIND THE DRIVE**, and
/// both of those are declared here rather than worked around. The bootloader checks each block's
/// magic, family id and index before accepting a byte, which is a real check on the IMAGE -- it is
/// not a check that the image reached the flash, because nothing reads the flash back.
///
/// The volume is chosen in [`identify`](FlashBackend::identify) rather than at construction, which
/// is what makes the ambiguity refusal safe: two RP2350s in BOOTSEL mount as two drives with the
/// same label and byte-identical `INFO_UF2.TXT` files, so nothing readable tells them apart. The
/// contract calls `identify` before `program`, so a bench holding two of them is refused before any
/// file is written rather than after.
pub struct Uf2Volume {
    requested: Option<String>,
    base: u32,
    family: u32,
    chosen: Option<std::path::PathBuf>,
}

impl Uf2Volume {
    /// A volume backend for a board written at `base`, whose bootloader takes `family`.
    ///
    /// `requested` names one volume when the caller already knows which -- on a bench that is
    /// the disk serial behind the drive, which is a fact this code cannot obtain for itself.
    pub fn new(requested: Option<&str>, base: u32, family: u32) -> Self {
        Self { requested: requested.map(str::to_owned), base, family, chosen: None }
    }
}

impl FlashBackend for Uf2Volume {
    fn mechanism(&self) -> &'static str {
        "the board's bootloader volume, by copying the image"
    }

    fn flash_base(&self) -> u32 {
        self.base
    }

    /// Settle WHICH volume, and refuse rather than guess.
    ///
    /// The [`PartIdentity`] this returns is the UF2 family id, because that is the only fact this
    /// route actually establishes -- and `what` says so, since a family id is shared by every board
    /// of that family and settles nothing about which one is attached.
    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        let mounted: Vec<crate::bootsel::Waiting> = crate::bootsel::waiting()
            .into_iter()
            .filter(|found| found.via == crate::bootsel::Via::Bootloader)
            .collect();
        let volume = match &self.requested {
            Some(named) => named.clone(),
            None => match mounted.as_slice() {
                [] => {
                    return Err(FlashError::Refused(
                        "no board is in its bootloader. Hold BOOTSEL while plugging the board in                          (or press RESET
with BOOTSEL held), and it will appear as a drive."
                            .to_owned(),
                    ));
                }
                [only] => only.volume.clone(),
                several => {
                    let list: Vec<&str> =
                        several.iter().map(|found| found.volume.as_str()).collect();
                    return Err(FlashError::Refused(format!(
                        "{} boards are in their bootloader and nothing on a volume tells them                          apart: {}

Name one with --probe <volume>. Their labels and their                          INFO_UF2.TXT files are identical, so
this will not guess -- the wrong                          choice puts your program on somebody else's board.",
                        several.len(),
                        list.join(", ")
                    )));
                }
            },
        };
        self.chosen = Some(std::path::PathBuf::from(volume));
        Ok(PartIdentity {
            value: u64::from(self.family),
            what: "a UF2 family, which every board of that family shares and which settles                    nothing about WHICH board is attached",
        })
    }

    /// Nothing, and that is the mechanism rather than an omission.
    ///
    /// A UF2 bootloader erases each sector as it programs it. There is no separate erase to
    /// perform and no way to ask for one, so this is complete rather than unimplemented.
    fn erase(&mut self, _image: &Image<'_>) -> Result<(), FlashError> {
        Ok(())
    }

    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let volume = self
            .chosen
            .as_ref()
            .ok_or_else(|| FlashError::Refused("no volume was settled".to_owned()))?;
        let destination = volume.join("lamella.uf2");
        write_through(&destination, image.bytes).map_err(|error| {
            FlashError::Refused(format!("copying to {}: {error}", destination.display()))
        })
    }

    /// [`None`] -- the volume is write-only.
    ///
    /// **A DRIVE THAT ACCEPTS A FILE IS NOT A WINDOW ONTO THE FLASH.** The bootloader consumes the
    /// blocks and unmounts; there is no path back through it to the programmed bytes. Answering
    /// `None` is what makes the report say so instead of claiming a verification.
    fn read_back(&mut self, _image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>> {
        None
    }

    /// Wait for the board to reboot, because that is this route's only evidence.
    ///
    /// **THE RP2350 DATASHEET STATES BOTH HALVES OF THIS.** On the completed download of an entire
    /// valid UF2, the chip reboots to run it (5.5.2); and when a download fails, *"it will appear
    /// as if nothing has happened since the device will not reboot"*. The same section warns that
    /// *"invalid UF2 files might not write at all or only write partially ... Not all operating
    /// systems notify you of disk write errors after a failed write."*
    ///
    /// So the drive going away IS the acknowledgement, and it is the only one this mechanism
    /// offers. It is not a read-back and this does not claim to be one -- nothing here has seen the
    /// programmed bytes. What it converts is a SILENT failure into a reported one.
    ///
    fn finish(&mut self) -> Result<(), FlashError> {
        let Some(volume) = self.chosen.clone() else { return Ok(()) };
        let marker = volume.join("INFO_UF2.TXT");
        if !marker.exists() {
            return Ok(());
        }
        for _ in 0..REBOOT_POLLS {
            if !marker.exists() {
                return Ok(());
            }
            std::thread::sleep(REBOOT_POLL_INTERVAL);
        }
        Err(FlashError::Refused(format!(
            "the image was copied but {} is still mounted, so the board did not reboot.\n\n\
A board that has accepted a complete UF2 reboots into it and the drive disappears. This\n\
one did not, which means the download did not complete -- a partial write, or an image\n\
the bootloader would not take. NOTHING WAS VERIFIED EITHER WAY: this route cannot read\n\
the flash.\n\n\
If this board is configured not to reboot after a download, that is the one innocent\n\
reason for this message. Otherwise the program is not on the board.",
            volume.display()
        )))
    }
}

/// How long to wait for the board to reboot before calling the download failed.
///
/// Generous rather than tight: the wait costs a person nothing on success, because the drive
/// disappears the moment the bootloader is satisfied and the poll returns immediately.
const REBOOT_POLLS: u32 = 100;

/// The gap between polls for the volume going away.
const REBOOT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Write `bytes` to `path` and make sure they have actually reached the device.
///
/// **`fs::write` IS NOT ENOUGH HERE AND THE FAILURE IS SILENT.** It writes and closes, which hands
/// the data to the operating system; on a removable volume the operating system is entitled to
/// hold it in cache. A bootloader volume is not a disk -- it is a device watching for blocks, and
/// it acts the moment they arrive. Without a flush the copy reports success, the file stays in the
/// directory listing, and the board stays in its bootloader, because nothing has been delivered:
/// every layer reports success and nothing happens.
///
/// `sync_all` is the difference: it flushes the file's buffers through to the device before the
/// call returns, so a success here means the board has the bytes.
fn write_through(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// An STM32L0 over any probe, driven by the part's own flash controller.
///
/// The STM32 crate exposes unlock, erase-a-page and program-words and no orchestrator, which is the
/// shape this module's header says a part crate should have -- so the whole of the sequencing is
/// here, and none of it is duplicated there.
///
/// Generic over the target for the same reason its siblings are: the controller routines are
/// written against [`TargetAccess`], so an L0 reached by an ST-Link and one reached by a CMSIS-DAP
/// probe take the same path, and a test can drive the sequence with no hardware at all.
///
/// # What this part makes different from its siblings here
///
/// **It erases to ZERO.** Every other part this tree programs erases to ones, so a blank check, a
/// post-erase verify or a padding assumption carried over from one of them is exactly backwards
/// here. Nothing in this backend spells an erased value: [`Stm32L0Flash`] holds it, and the
/// short-tail padding below relies on it deliberately rather than by accident.
///
/// **The cost of a mistake varies by product category, and the part will say which it is.** On a
/// category 3 device a program to a word that is not zero is CARRIED OUT, ORing old with new
/// including the ECC, after which the cell cannot be read back correctly; on every other category
/// the write is discarded (RM0367 3.3.4). This backend never programs without erasing first, so it
/// does not depend on that difference -- but [`identify`](FlashBackend::identify) reports the
/// category, because a caller deciding whether to retry a failed write needs to know which part it
/// is holding.
///
/// # The connect happens before construction
///
/// Same as [`Rp2350Probe`]: the probe is opened, brought into SWD and given memory access by the
/// caller, and this type takes it from there. The first thing it does is a read that touches
/// nothing, so identify-before-erase holds regardless.
pub struct Stm32L0Probe<A: TargetAccess> {
    target: A,
}

impl<A: TargetAccess> Stm32L0Probe<A> {
    /// A backend for an STM32L0 reached through `target`.
    pub fn new(target: A) -> Self {
        Stm32L0Probe { target }
    }

    /// What a `DEV_ID` reading settles, said plainly enough that a caller cannot mistake it for
    /// more.
    ///
    /// **IT NAMES A CATEGORY AND NOT A BOARD**, and the contract's sixth prohibition is exactly
    /// about not letting that pass unsaid. Every STM32L073 and STM32L083 on a bench answers
    /// `0x447`, so this settles which flash behaviour applies and settles nothing about WHICH of
    /// them is on the wire.
    fn what(category: Stm32L0Category) -> &'static str {
        match category {
            Stm32L0Category::One => "an STM32L0 category 1 part -- the category, which every part in it answers, not this board",
            Stm32L0Category::Two => "an STM32L0 category 2 part -- the category, which every part in it answers, not this board",
            Stm32L0Category::Three => "an STM32L0 category 3 part -- the category, which every part in it answers, not this board",
            Stm32L0Category::Five => "an STM32L0 category 5 part -- the category, which every part in it answers, not this board",
        }
    }

    /// Read back exactly the bytes `image` covers. The core is already halted by
    /// [`erase`](FlashBackend::erase) and stays halted until [`finish`](FlashBackend::finish).
    fn read_span(&mut self, image: &Image<'_>) -> Result<Vec<u8>, FlashError> {
        let words = image.bytes.len().div_ceil(4);
        let read = self.target.read_words(image.base, words)?;
        let mut bytes = Vec::with_capacity(words * 4);
        for word in read {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.truncate(image.bytes.len());
        Ok(bytes)
    }
}

impl<A: TargetAccess> FlashBackend for Stm32L0Probe<A> {
    fn mechanism(&self) -> &'static str {
        "an SWD probe, by the part's own flash controller"
    }

    fn flash_base(&self) -> u32 {
        STM32L0_FLASH_BASE
    }

    /// `DBGMCU_IDCODE`, which is the reading that names ST's die.
    ///
    /// **A DEBUG-PORT IDCODE IS NOT AN IDENTITY ON THIS PART.** `0x0bc11477` is Arm's M0-class
    /// SW-DP and an STM32C0 or a SAM D11 answers it as readily as an L0, so a backend keying on it
    /// would confirm nothing while sounding like it had. `DEV_ID` is ST's own, and an id in
    /// neither manual's list is refused here rather than carried into an erase: `--part l0` aimed
    /// at a foreign die would put two unlock key sequences at `0x40022000` on a part where that
    /// address is something else.
    ///
    /// Costs the board nothing: no halt, no clock enabled, core still running.
    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        let (dev_id, _rev_id) = stm32l0_dev_id(&mut self.target)?;
        match Stm32L0Category::from_dev_id(dev_id) {
            Some(category) => {
                Ok(PartIdentity { value: u64::from(dev_id), what: Self::what(category) })
            }
            None => Err(FlashError::Refused(format!(
                "DBGMCU_IDCODE reports DEV_ID {dev_id:#05x}, which is no STM32L0 category. \
                 RM0377 27.4.1 lists 0x457, 0x425, 0x417 and 0x447."
            ))),
        }
    }

    /// Ask the part how big it is, then halt, unlock, and erase the pages the image covers.
    ///
    /// **THE SIZE COMES FROM THE PART, NOT FROM THE CALLER.** A host tool cannot see how much flash
    /// is fitted; told the wrong thing, it erases and programs past the end of the array one page
    /// at a time and reports success on every page that happened to exist. `F_SIZE` is
    /// factory-programmed and the part answers it (RM0377 and RM0367 34.1.1).
    ///
    /// **AND THE PAGE WALK REACHES BOTH BANKS OF A DUAL-BANK PART.** A 192 KB category 5 device is
    /// two banks, but they are contiguous -- Bank 1 `0x08000000`-`0x08017FFF`, Bank 2 immediately
    /// after -- and the controller takes an address rather than a bank, so a linear walk crosses
    /// the join without selecting anything. Measured on a NUCLEO-L073RZ: one program spanning
    /// `0x08017FF8`-`0x08018008` read back unchanged, against a control at an ordinary page join.
    fn erase(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let fitted = stm32_flash_size_bytes(&mut self.target, STM32L0_FLASH_SIZE_REG)?;
        let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        if wanted > fitted {
            return Err(FlashError::Refused(format!(
                "the image is {wanted} bytes and the part reports {} KB of flash",
                fitted / 1024
            )));
        }
        self.target.halt()?;
        self.target.l0_unlock_flash()?;
        let pages = wanted.div_ceil(STM32L0_PAGE);
        for page in 0..pages {
            self.target.l0_erase_page(image.base + page * STM32L0_PAGE)?;
        }
        self.target.l0_lock_flash()?;
        Ok(())
    }

    /// Program the image, word at a time.
    ///
    /// **A SHORT TAIL IS PADDED WITH ZERO, AND ON THIS FAMILY THAT IS THE ERASED VALUE.** So the
    /// padding is not written at all -- the programmer skips a zero word because the cell already
    /// holds one -- and those cells stay erased rather than being programmed with filler. The same
    /// line on a ones-erasing part would be a defect.
    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        self.target.l0_unlock_flash()?;
        let mut padded = image.bytes.to_vec();
        while padded.len() % 4 != 0 {
            padded.push(0);
        }
        let programmed = self.target.l0_program(image.base, &padded);
        let locked = self.target.l0_lock_flash();
        programmed?;
        locked?;
        Ok(())
    }

    /// Every byte, read back over the same wire that wrote them.
    ///
    /// A probe has a read-back, so this must use it: `None` here is a statement that the MECHANISM
    /// has none, and would be false.
    fn read_back(&mut self, image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>> {
        Some(self.read_span(image))
    }

    fn finish(&mut self) -> Result<(), FlashError> {
        self.target.reset_and_run()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
