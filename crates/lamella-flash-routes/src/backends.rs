//! The flashing backends this lane owns, implemented against the contract.

use lamella_cmsis_dap_nrf::Nrf51Flash;
use lamella_cmsis_dap_sam::{
    SAM3X_CHIPID_CIDR, SAM3X_EEFC0, SAM3X_EEFC1, SAM3X_FLASH0_BASE, SAM3X_FLASH1_BASE,
    SAM3X_GPNVM_PLANE_SWAP, SAM3X_LOCK_PAGES, SAM3X_PAGE, SAM3X_PLANE_SIZE, SAM4E_EEFC,
    SAM4E_FLASH_BASE, SAM4L_FLASH_BASE, SAM4L_LOCK_REGIONS, SAM4S_EEFC0, SAM4S_EEFC1,
    SAM4S_ERASE_PAGES, SAM4S_FLASH0_BASE, SAM4S_FLASH1_BASE, SAM4S_GPNVM_PLANE_SWAP,
    SAM4S_LOCK_PAGES, SAM4S_PAGE, SAM4_CHIPID_CIDR, SAM4_CHIPID_EXID, SAME54_BLOCK, Sam3xFlash,
    Sam4lFlash, Sam4sFlash, Samd21Flash, SamIdentify, Same54Flash, sam3x_identify,
    sam4_family_matches, sam4_identify,
};
use lamella_cmsis_dap_stm32::{
    STM32C0_DBGMCU_IDCODE, STM32C0_DOUBLE_WORD, STM32C0_ERASED_VALUE, STM32C0_FLASH_BASE,
    STM32C0_FLASH_SIZE_REG, STM32C0_PAGE, STM32C0_PARTS, STM32H7_BANK2_BASE, STM32H7_DBGMCU_IDC,
    STM32H7_FLASH_BASE, STM32H7_FLASH_SIZE_REG, STM32H7_FLASH_WORD, STM32H7_PARTS, STM32H7_SECTOR,
    STM32L0_DBGMCU_IDCODE, STM32L0_ERASED_WORD, STM32L0_FLASH_BASE, STM32L0_FLASH_SIZE_REG,
    STM32L0_PAGE, STM32L0_PARTS, STM32L4_DBGMCU_IDCODE, STM32L4_DOUBLE_WORD, STM32L4_ERASED_WORD,
    STM32L4_FLASH_BASE, STM32L4_FLASH_SIZE_REG, STM32L4_PAGE, STM32L4_PARTS, STM32U5_DBGMCU_IDCODE,
    STM32U5_FLASH_BASE, STM32U5_FLASH_SIZE_REG, STM32U5_PAGE, STM32U5_PARTS, STM32U5_QUAD_WORD,
    Stm32C0Flash, Stm32H7Flash, Stm32L0Flash, Stm32L4Flash, Stm32U5Flash, stm32_dev_id,
    stm32_flash_size_bytes,
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
        MicrobitDaplink {
            target,
            expect: PartIdentity {
                value: u64::from(idcode),
                what,
            },
        }
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
            return Err(FlashError::WrongPart {
                expected: self.expect.clone(),
                found,
            });
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
        Nrf51Flash::write_flash(&mut self.target, image.base, &words)?;
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
        Rp2350Probe {
            target,
            expect: PartIdentity {
                value: u64::from(idcode),
                what,
            },
        }
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
        Ok(PartIdentity {
            value: chip_id,
            what: self.expect.what,
        })
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

/// An RP2040 over an SWD probe, programmed by the chip's own bootrom flash API.
///
/// **THE ALTERNATIVE TO THE BOOTLOADER VOLUME, AND THE ONLY ROUTE TO THIS PART THAT CAN VERIFY.**
/// A volume write cannot be read back at all; this reads every byte through the execute-in-place
/// window and compares it.
///
/// Coarse in the same way [`Rp2350Probe`] is, and for the same reason: the part crate's
/// `flash_image` performs reset-halt, erase, program and its own verify in one call, so
/// [`erase`](FlashBackend::erase) here is empty and the erase happens inside
/// [`program`](FlashBackend::program). The contract's ORDER still holds --
/// [`identify`](FlashBackend::identify) runs before `program` is ever called.
///
pub struct Rp2040Probe<A: TargetAccess> {
    target: A,
    expect: PartIdentity,
}

impl<A: TargetAccess> Rp2040Probe<A> {
    /// A backend for a part whose debug port answered `idcode`.
    ///
    /// The connect happens before construction because the part crate's `connect` is concrete over
    /// its transport rather than generic over [`TargetAccess`] -- selecting one debug port out of
    /// the several this part puts on one SWD bus is a wire-level operation. So the reading is taken
    /// first and checked here.
    pub fn new(target: A, idcode: u32, what: &'static str) -> Self {
        Rp2040Probe {
            target,
            expect: PartIdentity {
                value: u64::from(idcode),
                what,
            },
        }
    }
}

impl<A: TargetAccess> FlashBackend for Rp2040Probe<A> {
    fn mechanism(&self) -> &'static str {
        "an SWD probe, by the chip's own bootrom flash API"
    }

    fn flash_base(&self) -> u32 {
        lamella_cmsis_dap_rp2040::XIP_BASE
    }

    /// The debug port's own id, read again rather than repeated from the connect.
    ///
    /// **THERE IS NO BOARD IDENTITY TO READ ON THIS PART, and this says so rather than dressing a
    /// family id as one.** The RP2350 sibling answers with a 64-bit OTP chip id no other board
    /// shares; an RP2040 has no OTP at all -- its unique id belongs to the QSPI flash device rather
    /// than the die. So what this settles is the GENERATION: it stops a Pico 2's image reaching a
    /// Pico, which is the mix-up that erases a board, and it does not stop one Pico's image
    /// reaching another. On a bench holding several, the probe serial and the wiring are what name
    /// the board, and nothing here can check them.
    ///
    /// It reads WITHOUT halting the core, so identifying costs the board nothing and happens before
    /// anything is erased.
    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        let found = u64::from(self.target.read_idcode()?);
        if found != self.expect.value {
            return Err(FlashError::WrongPart {
                expected: self.expect.clone(),
                found,
            });
        }
        Ok(self.expect.clone())
    }

    /// Nothing: the erase happens inside [`program`](Self::program).
    ///
    /// See the type's own note. This is empty because the part crate does not offer an erase that
    /// can be called on its own, not because an RP2040 needs no erasing.
    fn erase(&mut self, _image: &Image<'_>) -> Result<(), FlashError> {
        Ok(())
    }

    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        lamella_cmsis_dap_rp2040::flash_image(&mut self.target, image.bytes, |line| {
            println!("  {line}");
        })?;
        Ok(())
    }

    /// Every byte, read back through the execute-in-place window.
    ///
    /// **THIS IS A SECOND, INDEPENDENT CHECK.** `flash_image` verifies internally as it programs;
    /// this reads the flash again afterwards and lets the contract do the comparison. The two are
    /// deliberately not folded together -- a verify inside the routine that did the writing shares
    /// every assumption the writing made, and the point of the outer one is that it does not.
    fn read_back(&mut self, image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>> {
        Some(self.read_span(image))
    }

    fn finish(&mut self) -> Result<(), FlashError> {
        self.target.reset_and_run()?;
        Ok(())
    }
}

impl<A: TargetAccess> Rp2040Probe<A> {
    /// Read back exactly the bytes `image` covers.
    ///
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
    /// `requested` names one volume when the caller already knows which -- in practice that is
    /// the disk serial behind the drive, which is a fact this code cannot obtain for itself.
    pub fn new(requested: Option<&str>, base: u32, family: u32) -> Self {
        Self {
            requested: requested.map(str::to_owned),
            base,
            family,
            chosen: None,
        }
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
        let Some(volume) = self.chosen.clone() else {
            return Ok(());
        };
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

/// Everything an STM32 family's flash write needs EXCEPT the sequence.
///
/// **THE SEQUENCE IS THE PART WORTH SHARING AND THE NUMBERS ARE THE PART WORTH SEPARATING.** Ask the
/// part its size, refuse an image that does not fit, halt, unlock, erase the granules the image
/// covers, lock, unlock, program, lock, read back, reset -- that order is identical on every family
/// here, and it is the order that goes wrong. What differs is a handful of addresses, a granule size
/// and an erased value, which is what this carries.
///
/// **EVERY FIELD IS FILLED FROM A `lamella-cmsis-dap-stm32` CONSTANT AND NEVER FROM A LITERAL**, so
/// the part crate stays the one place a register address is written down. A plan that spelled its
/// own `0x0800_0000` would be a second source for a number the chip crate already owns, and the two
/// would diverge in the direction nothing reads.
pub struct StPlan {
    /// Which family this describes. The only field the primitive dispatch reads.
    pub family: crate::StFamily,
    /// Where the flash array is mapped for execution.
    pub flash_base: u32,
    /// The factory-programmed flash-size register, read through [`stm32_flash_size_bytes`].
    pub size_register: u32,
    /// The DBGMCU identity register, read through [`stm32_dev_id`].
    pub id_register: u32,
    /// The `DEV_ID` values this family's manual lists, with what each names.
    pub parts: &'static [(u32, &'static str)],
    /// One erase operation's span: a page on the L0 and the U5, a sector on the H7.
    pub erase_granule: u32,
    /// What an erased cell reads as.
    ///
    /// **The L0 is the only part here that erases to ZERO**, which is why this is a field and not an
    /// assumption -- a padding rule carried over from a ones-erasing sibling programs the L0's tail
    /// instead of leaving it erased.
    pub erased_word: u32,
    /// The alignment a program must start on and the granule a short tail is padded up to.
    pub program_align: u32,
    /// Where each independently LOCKED bank begins.
    ///
    /// **ONE ENTRY MEANS ONE LOCK FOR THE WHOLE ARRAY**, which is the L0's and the U5's shape. The
    /// H7 has two complete register sets and unlocking bank 1 leaves bank 2 locked, so a write
    /// spanning the join has to take both -- and a bank-2 address driven through bank 1's registers
    /// does nothing and reports success.
    pub banks: &'static [u32],
    /// Whether this family has to be attached with NRST asserted rather than through the plain
    /// SWD entry.
    ///
    /// **A PART THAT NEEDS THIS DOES NOT FAIL AT THE ATTACH; IT FAILS AT EVERY MEMORY ACCESS AFTER
    /// ONE THAT SUCCEEDED.** A running STM32H755 answers `READ_IDCODE` with `0x6ba02477` and then
    /// refuses every read, `INIT_AP` included -- so the debug port looks healthy and the first whole
    /// flash read comes back 2 MB of `0xFF` with every chunk failed. A backend that trusted the
    /// idcode would report an erased part rather than a refused one.
    ///
    /// False everywhere else here, because the plain path is what those families were driven with
    /// and an attach is not the place to change a proven route's behavior speculatively.
    pub attach_under_reset: bool,
    /// What one [`program_align`](Self::program_align) chunk of this route is CALLED, for a person
    /// watching the write count them.
    pub unit: &'static str,
    /// The manual every number above is read from, named in the refusal so a reader can check it.
    pub manual: &'static str,
}

/// The STM32L0: 128-byte pages, one lock, and the only part here that erases to zero.
///
/// **The cost of a mistake varies by product category, and the part will say which it is.** On a
/// category 3 device a program to a word that is not zero is CARRIED OUT, ORing old with new
/// including the ECC, after which the cell cannot be read back correctly; on every other category
/// the write is discarded (RM0367 3.3.4). This backend never programs without erasing first, so it
/// does not depend on that difference -- but [`identify`](FlashBackend::identify) reports the
/// category, because a caller deciding whether to retry a failed write needs to know which part it
/// is holding.
const L0_PLAN: StPlan = StPlan {
    family: crate::StFamily::L0,
    flash_base: STM32L0_FLASH_BASE,
    size_register: STM32L0_FLASH_SIZE_REG,
    id_register: STM32L0_DBGMCU_IDCODE,
    parts: STM32L0_PARTS,
    erase_granule: STM32L0_PAGE,
    erased_word: STM32L0_ERASED_WORD,
    program_align: 4,
    banks: &[STM32L0_FLASH_BASE],
    attach_under_reset: false,
    unit: "words",
    manual: "RM0377 27.4.1",
};

/// The STM32C0: 2 KB pages, a 64-bit double word, one lock.
///
/// **THE ERASED VALUE IS A REGISTER'S OWN DEFINITION HERE, NOT A SENTENCE.** RM0490 states the
/// reprogram rule through `FLASH_SR`: a double word is write-once between erases, except that
/// writing all zeroes writes no information -- which is the controller defining "erased" in the
/// register whose job is to decide whether a write may proceed.
const C0_PLAN: StPlan = StPlan {
    family: crate::StFamily::C0,
    flash_base: STM32C0_FLASH_BASE,
    size_register: STM32C0_FLASH_SIZE_REG,
    id_register: STM32C0_DBGMCU_IDCODE,
    parts: STM32C0_PARTS,
    erase_granule: STM32C0_PAGE,
    erased_word: STM32C0_ERASED_VALUE,
    program_align: STM32C0_DOUBLE_WORD as u32,
    banks: &[STM32C0_FLASH_BASE],
    attach_under_reset: false,
    unit: "double words",
    manual: "RM0490 Table 178",
};

/// The STM32L4: 2 KB pages, a 64-bit double word, two banks behind ONE lock.
///
/// **ITS IDENTITY REGISTER IS THE F4/F7 DEBUG-REGION ADDRESS AND NOT ITS L0 SIBLING'S**, which is the
/// single most borrowable number in this table and the one a family-by-name guess gets wrong.
const L4_PLAN: StPlan = StPlan {
    family: crate::StFamily::L4,
    flash_base: STM32L4_FLASH_BASE,
    size_register: STM32L4_FLASH_SIZE_REG,
    id_register: STM32L4_DBGMCU_IDCODE,
    parts: STM32L4_PARTS,
    erase_granule: STM32L4_PAGE,
    erased_word: STM32L4_ERASED_WORD,
    program_align: STM32L4_DOUBLE_WORD as u32,
    banks: &[STM32L4_FLASH_BASE],
    attach_under_reset: false,
    unit: "double words",
    manual: "RM0351",
};

/// The STM32H7: 128 KB sectors, a 32-byte flash word, and TWO independently locked banks.
///
const H7_PLAN: StPlan = StPlan {
    family: crate::StFamily::H7,
    flash_base: STM32H7_FLASH_BASE,
    size_register: STM32H7_FLASH_SIZE_REG,
    id_register: STM32H7_DBGMCU_IDC,
    parts: STM32H7_PARTS,
    erase_granule: STM32H7_SECTOR,
    erased_word: 0xffff_ffff,
    program_align: STM32H7_FLASH_WORD as u32,
    banks: &[STM32H7_FLASH_BASE, STM32H7_BANK2_BASE],
    attach_under_reset: true,
    unit: "flash words",
    manual: "RM0399",
};

/// The STM32U5: 8 KB pages, a 128-bit quad-word, and one lock for both banks.
///
/// **TWO BANKS BUT ONE LOCK**, unlike the H7: `FLASH_NSCR` covers the array and `u5_erase_page`
/// selects the bank itself, so a single entry here is the U5's real shape and not a simplification.
const U5_PLAN: StPlan = StPlan {
    family: crate::StFamily::U5,
    flash_base: STM32U5_FLASH_BASE,
    size_register: STM32U5_FLASH_SIZE_REG,
    id_register: STM32U5_DBGMCU_IDCODE,
    parts: STM32U5_PARTS,
    erase_granule: STM32U5_PAGE,
    erased_word: 0xffff_ffff,
    program_align: STM32U5_QUAD_WORD as u32,
    banks: &[STM32U5_FLASH_BASE],
    attach_under_reset: false,
    unit: "quad-words",
    manual: "RM0456 75.12.4",
};

impl crate::StFamily {
    /// The numbers this family's flash write needs.
    ///
    /// **A `match` WITH NO DEFAULT ARM, DELIBERATELY.** Adding a variant to [`crate::StFamily`]
    /// without a plan is then a compile error rather than a route that resolves at run time to
    /// somebody else's register addresses.
    pub fn plan(self) -> &'static StPlan {
        match self {
            crate::StFamily::L0 => &L0_PLAN,
            crate::StFamily::C0 => &C0_PLAN,
            crate::StFamily::L4 => &L4_PLAN,
            crate::StFamily::H7 => &H7_PLAN,
            crate::StFamily::U5 => &U5_PLAN,
        }
    }
}

/// An STM32 over any probe, driven by the part's own flash controller.
///
/// The STM32 crate exposes unlock, erase-a-granule and program and no orchestrator, which is the
/// shape this module's header says a part crate should have -- so the whole of the sequencing is
/// here, and none of it is duplicated there.
///
/// Generic over the target for the same reason its siblings are: the controller routines are
/// written against [`TargetAccess`], so a part reached by an ST-Link and one reached by a CMSIS-DAP
/// probe take the same path, and a test can drive the sequence with no hardware at all.
///
/// # One backend, several families, and why that is not a stretch
///
/// **The sequencing is identical and only the register addresses and the geometry differ**, which
/// [`StPlan`] carries. The four primitive calls dispatch on the family because the part crate gives
/// each one its own trait method name; everything around them is written once. That is the whole
/// reason to do it this way: the ORDER is what has repeatedly gone wrong in this tree, and a repair
/// to a shared order reaches every family, where a repair to the third of four copies does not.
///
/// # The connect happens before construction
///
/// Same as [`Rp2350Probe`]: the probe is opened, brought into SWD and given memory access by the
/// caller, and this type takes it from there. The first thing it does is a read that touches
/// nothing, so identify-before-erase holds regardless.
pub struct StProbe<A: TargetAccess> {
    target: A,
    plan: &'static StPlan,
}

impl<A: TargetAccess> StProbe<A> {
    /// A backend for the family `plan` describes, reached through `target`.
    pub fn new(target: A, plan: &'static StPlan) -> Self {
        StProbe { target, plan }
    }

    /// Takes the lock covering `at`. Idempotent on all three families, and it has to be: every one
    /// of them locks its control register until the next system reset if the key sequence is
    /// performed a SECOND time, so each primitive reads the lock bit before writing a key.
    fn unlock(&mut self, at: u32) -> Result<(), FlashError> {
        match self.plan.family {
            crate::StFamily::L0 => self.target.l0_unlock_flash()?,
            crate::StFamily::C0 => self.target.c0_unlock_flash()?,
            crate::StFamily::L4 => self.target.l4_unlock_flash()?,
            crate::StFamily::H7 => self.target.h7_unlock_flash(at)?,
            crate::StFamily::U5 => self.target.u5_unlock_flash()?,
        }
        Ok(())
    }

    /// Re-takes the lock covering `at`.
    fn lock(&mut self, at: u32) -> Result<(), FlashError> {
        match self.plan.family {
            crate::StFamily::L0 => self.target.l0_lock_flash()?,
            crate::StFamily::C0 => self.target.c0_lock_flash()?,
            crate::StFamily::L4 => self.target.l4_lock_flash()?,
            crate::StFamily::H7 => self.target.h7_lock_flash(at)?,
            crate::StFamily::U5 => self.target.u5_lock_flash()?,
        }
        Ok(())
    }

    /// Erases the one granule containing `at`.
    fn erase_granule(&mut self, at: u32) -> Result<(), FlashError> {
        match self.plan.family {
            crate::StFamily::L0 => self.target.l0_erase_page(at)?,
            crate::StFamily::C0 => self.target.c0_erase_page(at)?,
            crate::StFamily::L4 => self.target.l4_erase_page(at)?,
            crate::StFamily::H7 => self.target.h7_erase_sector(at)?,
            crate::StFamily::U5 => self.target.u5_erase_page(at)?,
        }
        Ok(())
    }

    /// Programs `data` from `at`, which the caller has already padded to the family's granule.
    fn program_from(&mut self, at: u32, data: &[u8]) -> Result<(), FlashError> {
        match self.plan.family {
            crate::StFamily::L0 => self.target.l0_program(at, data)?,
            crate::StFamily::C0 => self.target.c0_program(at, data)?,
            crate::StFamily::L4 => self.target.l4_program(at, data)?,
            crate::StFamily::H7 => self.target.h7_program(at, data)?,
            crate::StFamily::U5 => self.target.u5_program(at, data)?,
        }
        Ok(())
    }

    /// Which of the family's locked banks an image at `base` of `len` bytes reaches.
    ///
    /// **A SINGLE-BANK FAMILY ALWAYS ANSWERS ITS ONE BANK, WHATEVER THE ADDRESS**, because its
    /// primitives ignore the address entirely -- the entry is a place to hang the one lock, not a
    /// range to test against. Testing it as a range would let an image based somewhere the plan does
    /// not name come back with NO banks, and a write that unlocks nothing fails after the erase.
    fn banks_covering(&self, base: u32, len: u32) -> Vec<u32> {
        if self.plan.banks.len() == 1 {
            return vec![self.plan.banks[0]];
        }
        let end = base.saturating_add(len);
        let mut taken = Vec::new();
        for (index, start) in self.plan.banks.iter().copied().enumerate() {
            let next = self.plan.banks.get(index + 1).copied().unwrap_or(u32::MAX);
            if start < end && base < next {
                taken.push(start);
            }
        }
        taken
    }

    /// The byte a short tail is padded with: whatever an erased cell of this family already holds.
    ///
    /// **SO PADDING IS NEVER A WRITE.** On the L0 that means a zero tail is skipped by the
    /// programmer and those cells stay erased; on a ones-erasing part the same rule pads with `0xFF`
    /// and writes the flash word once. The same line would be a defect on either part with the other
    /// one's value hard-coded, which is why it is derived from the plan.
    fn erased_byte(&self) -> u8 {
        (self.plan.erased_word & 0xff) as u8
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

impl<A: TargetAccess> FlashBackend for StProbe<A> {
    fn mechanism(&self) -> &'static str {
        "an SWD probe, by the part's own flash controller"
    }

    fn flash_base(&self) -> u32 {
        self.plan.flash_base
    }

    /// The family's DBGMCU identity register, which is the reading that names ST's die.
    ///
    /// **A DEBUG-PORT IDCODE IS NOT AN IDENTITY ON ANY OF THESE PARTS.** `0x0bc11477` is Arm's
    /// M0-class SW-DP and an STM32C0 or a SAM D11 answers it as readily as an L0, so a backend
    /// keying on it would confirm nothing while sounding like it had. `DEV_ID` is ST's own, and an
    /// id the family's manual does not list is refused here rather than carried into an erase: a
    /// route aimed at a foreign die would put unlock key sequences at addresses that are something
    /// else on that part.
    ///
    /// **AND IT NAMES A GROUP, NOT A BOARD.** Every STM32L073 and L083 answers `0x447`; every
    /// H745/H747/H755/H757 answers `0x450`. The sentence each row carries says so, because the
    /// contract's sixth prohibition is about not letting that pass unsaid.
    ///
    /// Costs the board nothing: no halt, no clock enabled, core still running.
    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        let (dev_id, _rev_id) = stm32_dev_id(&mut self.target, self.plan.id_register)?;
        match self.plan.parts.iter().find(|(listed, _)| *listed == dev_id) {
            Some((_, what)) => Ok(PartIdentity {
                value: u64::from(dev_id),
                what,
            }),
            None => {
                let listed: Vec<String> = self
                    .plan
                    .parts
                    .iter()
                    .map(|(id, _)| format!("{id:#05x}"))
                    .collect();
                Err(FlashError::Refused(format!(
                    "DBGMCU reports DEV_ID {dev_id:#05x}, which is no {} device id. {} lists {}.",
                    self.plan.family.name(),
                    self.plan.manual,
                    listed.join(", ")
                )))
            }
        }
    }

    /// Ask the part how big it is, then halt, unlock, and erase the granules the image covers.
    ///
    /// **THE SIZE COMES FROM THE PART, NOT FROM THE CALLER.** A host tool cannot see how much flash
    /// is fitted; told the wrong thing, it erases and programs past the end of the array one granule
    /// at a time and reports success on every one that happened to exist. `F_SIZE` is
    /// factory-programmed and every family here answers it at its own address.
    ///
    /// **AND THE BOUND IS ON THE WALK, NOT ON THE IMAGE.** The granule walk starts at the image's
    /// base and rounds its last granule up, so what has to stay inside the array is where the walk
    /// REACHES. Checking the image's length instead would be the same test only while the caller
    /// guarantees the image starts at [`flash_base`](FlashBackend::flash_base) -- a guarantee that
    /// lives in another crate and would go on compiling after it was relaxed.
    ///
    /// **AND THE WALK REACHES BOTH BANKS OF A DUAL-BANK PART.** The controllers take an ADDRESS
    /// rather than a bank number, so a linear walk crosses a bank join without selecting anything.
    /// Measured on a NUCLEO-L073RZ, whose 192 KB category 5 device is two contiguous banks: one
    /// program spanning `0x08017FF8`-`0x08018008` read back unchanged, against a control at an
    /// ordinary page join.
    ///
    /// **What the walk does NOT carry across a join is the LOCK**, which is why
    /// [`banks_covering`](StProbe::banks_covering) is consulted separately: on the L0 and the C0
    /// one lock covers the array, and on the H7 unlocking bank 1 leaves bank 2 locked.
    fn erase(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let fitted = stm32_flash_size_bytes(&mut self.target, self.plan.size_register)?;
        let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        let granules = wanted.div_ceil(self.plan.erase_granule);
        let walk_end = image
            .base
            .saturating_add(granules.saturating_mul(self.plan.erase_granule));
        let array_end = self.plan.flash_base.saturating_add(fitted);
        if walk_end > array_end {
            return Err(FlashError::Refused(format!(
                "erasing {wanted} bytes from {:#010x} walks to {walk_end:#010x}, past the {} KB \
                 this part reports fitted, whose array ends at {array_end:#010x}",
                image.base,
                fitted / 1024
            )));
        }
        self.target.halt()?;
        let banks = self.banks_covering(image.base, walk_end.saturating_sub(image.base));
        for bank in &banks {
            self.unlock(*bank)?;
        }
        for granule in 0..granules {
            self.erase_granule(image.base + granule * self.plan.erase_granule)?;
        }
        for bank in &banks {
            self.lock(*bank)?;
        }
        Ok(())
    }

    /// Program the image, in whatever unit this family writes.
    ///
    /// **A SHORT TAIL IS PADDED WITH THE FAMILY'S OWN ERASED BYTE**, so the padding is never a write
    /// that had to happen: on the L0 that byte is zero and the programmer skips a zero word because
    /// the cell already holds one, leaving the tail erased; on a ones-erasing part the same rule
    /// pads with `0xFF` and the granule is written once. Either value hard-coded would be a defect
    /// on the other family -- see [`erased_byte`](StProbe::erased_byte).
    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        let banks = self.banks_covering(image.base, wanted);
        for bank in &banks {
            self.unlock(*bank)?;
        }
        let mut padded = image.bytes.to_vec();
        let filler = self.erased_byte();
        while padded.len() % self.plan.program_align as usize != 0 {
            padded.push(filler);
        }
        let programmed = self.program_from(image.base, &padded);
        let mut locked = Ok(());
        for bank in &banks {
            if let Err(why) = self.lock(*bank) {
                locked = locked.and(Err(why));
            }
        }
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

/// A Microchip SAM reached through its on-board EDBG, driven by the part's own flash controller.
///
/// # Why this is NOT the ST backend with different constants
///
/// [`StProbe`] shares one SEQUENCE across five families because they genuinely have one: ask the
/// size, halt, unlock, walk the granules, lock, program, read back, reset. **The SAM controllers do
/// not.** A SAM3X has no page erase at all -- only erase-all -- so it has no granule walk to share;
/// a SAM4L must invalidate its flash cache after a write or a correct write reads back as all ones
/// forever; an EEFC addresses by controller base and PAGE NUMBER rather than by address, and carries
/// lock bits that may have to be cleared first.
///
/// **So what is shared here is the CONTRACT, not the sequence**, and this type is honest about that:
/// [`read_back`](FlashBackend::read_back), [`finish`](FlashBackend::finish) and the identify-refusal
/// shape are written once, and erase and program dispatch to arms that are allowed to differ in
/// shape rather than only in constants. Forcing them into one walk is the mistake this lane already
/// made once by comparing two mechanisms and getting the comparison wrong.
///
/// # The connect happens before construction
///
/// As with every backend here: the probe is opened, brought into SWD and given memory access by the
/// caller. The first thing this does is a read that touches nothing.
pub struct SamProbe<A: TargetAccess> {
    target: A,
    family: crate::SamFamily,
    mechanism: &'static str,
}

impl<A: TargetAccess> SamProbe<A> {
    /// A backend for `family`, reached through `target`, describing itself as `mechanism`.
    ///
    /// **THE MECHANISM IS THE ROUTE'S FACT AND NOT THE FAMILY'S**, which is why it is passed in
    /// rather than derived here. The same controller is reached through a debugger soldered to the
    /// board on an Xplained kit and through a probe the reader supplied on an Arduino Due, and the
    /// sentence a person reads afterwards is about which of those happened.
    pub fn new(target: A, family: crate::SamFamily, mechanism: &'static str) -> Self {
        SamProbe { target, family, mechanism }
    }

    /// Read back exactly the bytes `image` covers, over the same wire that wrote them.
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

    /// Split a SAM3X image across the two planes, refusing every state that would put bytes
    /// somewhere other than where the image says.
    ///
    /// **THE TWO PLANES ARE CONTIGUOUS AND THE TWO CONTROLLERS ARE NOT**, which is the whole
    /// difficulty. `0x00080000` and `0x000C0000` sit next to each other, so a 512 KB image is one
    /// unbroken span to everything upstream of here and two entirely separate command sequences to
    /// the part. Nothing in the address tells the caller where the join is.
    ///
    /// **AND WHICH CONTROLLER FRONTS WHICH WINDOW IS A FUSE, NOT AN ADDRESS.** `GPNVM2` swaps them.
    /// This reads it rather than assuming the reset state, because getting it wrong fills one
    /// plane's latch buffer and programs the other -- and reports success, since every command
    /// completed.
    ///
    /// Returns one leg per controller the image reaches, in address order.
    fn sam3x_plan(&mut self, image: &Image<'_>) -> Result<Vec<Sam3xLeg>, FlashError> {
        let gpnvm = self.target.sam3x_gpnvm_bits()?;
        if gpnvm & (1 << SAM3X_GPNVM_PLANE_SWAP) != 0 {
            return Err(FlashError::Refused(format!(
                "GPNVM reads {gpnvm:#x}, and bit {SAM3X_GPNVM_PLANE_SWAP} -- the flash plane swap \
                 -- is SET: plane 1 is mapped at {SAM3X_FLASH0_BASE:#010x} and the two windows have \
                 exchanged controllers. This route drives the reset mapping, which is what a bare \
                 part is in; it refuses the swapped one rather than driving it untested, because \
                 the wrong controller fills one plane's latch and programs the other while every \
                 command reports success."
            )));
        }
        if image.base < SAM3X_FLASH0_BASE {
            return Err(FlashError::Refused(format!(
                "this image is based at {:#010x}, below the {SAM3X_FLASH0_BASE:#010x} where this \
                 part's flash begins",
                image.base
            )));
        }
        let page = SAM3X_PAGE as u32;
        let offset = image.base - SAM3X_FLASH0_BASE;
        if offset % page != 0 {
            return Err(FlashError::Refused(format!(
                "this image starts {offset:#x} into the array and this part's page is {page} \
                 bytes; an EWP erases and writes a whole page"
            )));
        }
        let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        let fitted = SAM3X_PLANE_SIZE.saturating_mul(2);
        let walk_end = offset.saturating_add(wanted.div_ceil(page).saturating_mul(page));
        if walk_end > fitted {
            return Err(FlashError::Refused(format!(
                "writing {wanted} bytes from {:#010x} reaches {:#010x}, past the {} KB this part \
                 fits across its two planes, which end at {:#010x}",
                image.base,
                SAM3X_FLASH0_BASE + walk_end,
                fitted / 1024,
                SAM3X_FLASH0_BASE + fitted
            )));
        }

        let mut legs = Vec::new();
        let mut at = offset;
        let end = offset + wanted;
        while at < end {
            let plane = at / SAM3X_PLANE_SIZE;
            let plane_end = (plane + 1) * SAM3X_PLANE_SIZE;
            let (eefc, plane_base) = if plane == 0 {
                (SAM3X_EEFC0, SAM3X_FLASH0_BASE)
            } else {
                (SAM3X_EEFC1, SAM3X_FLASH1_BASE)
            };
            let descriptor = self.target.sam3x_flash_descriptor(eefc)?;
            if descriptor.page_size != page || descriptor.plane_bytes != SAM3X_PLANE_SIZE {
                return Err(FlashError::Refused(format!(
                    "controller {eefc:#010x} reports a {}-byte page across a {} KB plane, and \
                     this route drives {page}-byte pages across {} KB. Its descriptor reports {} KB of flash in {} plane(s).",
                    descriptor.page_size,
                    descriptor.plane_bytes / 1024,
                    SAM3X_PLANE_SIZE / 1024,
                    descriptor.size / 1024,
                    descriptor.planes
                )));
            }
            let first_page = (at - plane * SAM3X_PLANE_SIZE) / page;
            let leg_end = end.min(plane_end);
            let last_page = (leg_end - 1 - plane * SAM3X_PLANE_SIZE) / page;
            let locked = self.target.sam3x_lock_bits(eefc)?;
            let region = |page_no: u32| page_no / SAM3X_LOCK_PAGES;
            let hit: Vec<u32> = (region(first_page)..=region(last_page))
                .filter(|bit| locked & (1 << bit) != 0)
                .collect();
            if !hit.is_empty() {
                return Err(FlashError::Refused(format!(
                    "controller {eefc:#010x} reports lock regions {hit:?} locked and this image \
                     covers them -- an EWP there raises FLOCKE and writes nothing. Its lock bits \
                     read {locked:#010x}, {SAM3X_LOCK_PAGES} pages to a region."
                )));
            }
            legs.push(Sam3xLeg {
                eefc,
                plane_base,
                first_page,
                bytes: (at - offset) as usize..(leg_end - offset) as usize,
            });
            at = leg_end;
        }
        Ok(legs)
    }
}

impl<A: TargetAccess> SamProbe<A> {
    /// Split a dual-plane SAM4S image across its two controllers, refusing every state that would
    /// put bytes somewhere other than where the image says.
    ///
    /// **THE SHAPE IS THE SAM3X's AND EVERY NUMBER IN IT IS DIFFERENT**, which is why this is
    /// written out rather than shared with it: the page is 512 bytes rather than 256, the erase is
    /// an eight-page block rather than folded into the write, a lock region is 16 pages rather than
    /// 64, and the planes are 1 MB rather than 256 KB. What the two share is the fuse.
    ///
    /// **PLANE 1's WINDOW IS DERIVED FROM PLANE 0's OWN REPORT, NOT FROM A PART TABLE.** An
    /// ATSAM4SD32's second plane is at `0x00500000` and an ATSAM4SD16's at `0x00480000`, and both
    /// are exactly one plane above the first -- so asking the controller how big its plane is
    /// answers for both parts, and the constant is what the answer is CHECKED against rather than
    /// what it is taken from.
    fn sam4s_dual_plan(&mut self, image: &Image<'_>) -> Result<Vec<Sam4sLeg>, FlashError> {
        let gpnvm = self.target.sam4s_gpnvm_bits()?;
        if gpnvm & (1 << SAM4S_GPNVM_PLANE_SWAP) != 0 {
            return Err(FlashError::Refused(format!(
                "GPNVM reads {gpnvm:#x}, and bit {SAM4S_GPNVM_PLANE_SWAP} -- the flash plane swap \
                 -- is SET: flash 1 is mapped in the {SAM4S_FLASH0_BASE:#010x} window and the two \
                 controllers have exchanged planes. This route drives the reset mapping and \
                 refuses the swapped one rather than driving it untested, because the wrong \
                 controller fills one plane's latch and programs the other while every command \
                 reports success."
            )));
        }
        let plane0 = self.target.sam4s_flash_descriptor(SAM4S_EEFC0)?;
        if plane0.planes != 2 {
            return Err(FlashError::Refused(format!(
                "this part reports {} flash plane(s) behind its first controller and this route \
                 drives the dual-plane SAM4S; a single-plane part is driven by the route that \
                 commands one EEFC and never reaches for a second",
                plane0.planes
            )));
        }
        let page = SAM4S_PAGE as u32;
        if plane0.page_size != page {
            return Err(FlashError::Refused(format!(
                "this part reports a {}-byte page and this route fills a {page}-byte latch buffer",
                plane0.page_size
            )));
        }
        let plane_size = plane0.plane_bytes;
        let plane1_base = SAM4S_FLASH0_BASE.saturating_add(plane_size);
        if plane1_base != SAM4S_FLASH1_BASE {
            return Err(FlashError::Refused(format!(
                "this part reports a {} KB first plane, which puts its second window at \
                 {plane1_base:#010x}; the SAM4SD32 this route drives has its second at {SAM4S_FLASH1_BASE:#010x}. Its descriptor reports {} KB of flash in {} plane(s).",
                plane_size / 1024,
                plane0.size / 1024,
                plane0.planes
            )));
        }
        if image.base < SAM4S_FLASH0_BASE {
            return Err(FlashError::Refused(format!(
                "this image is based at {:#010x}, below the {SAM4S_FLASH0_BASE:#010x} where this \
                 part's flash begins",
                image.base
            )));
        }
        let offset = image.base - SAM4S_FLASH0_BASE;
        let block = page * SAM4S_ERASE_PAGES;
        if offset % block != 0 {
            return Err(FlashError::Refused(format!(
                "this image starts {offset:#x} into the array and an EPA erase covers \
                 {SAM4S_ERASE_PAGES} pages of {page} bytes from a {block}-byte boundary"
            )));
        }
        let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        let fitted = plane_size.saturating_mul(2);
        let walk_end = offset.saturating_add(wanted.div_ceil(block).saturating_mul(block));
        if walk_end > fitted {
            return Err(FlashError::Refused(format!(
                "writing {wanted} bytes from {:#010x} erases to {:#010x}, past the {} KB this part \
                 fits across its two planes, which end at {:#010x}",
                image.base,
                SAM4S_FLASH0_BASE + walk_end,
                fitted / 1024,
                SAM4S_FLASH0_BASE + fitted
            )));
        }

        let mut legs = Vec::new();
        let mut at = offset;
        let end = offset + wanted;
        while at < end {
            let plane = at / plane_size;
            let plane_end = (plane + 1) * plane_size;
            let (eefc, plane_base) = if plane == 0 {
                (SAM4S_EEFC0, SAM4S_FLASH0_BASE)
            } else {
                (SAM4S_EEFC1, plane1_base)
            };
            let first_page = (at - plane * plane_size) / page;
            let leg_end = end.min(plane_end);
            let last_page = (leg_end - 1 - plane * plane_size) / page;
            let locked = self.target.sam4s_lock_bits(eefc)?;
            let bit_set = |region: u32| {
                locked
                    .get((region / 32) as usize)
                    .is_some_and(|word| word & (1 << (region % 32)) != 0)
            };
            let region = |page_no: u32| page_no / SAM4S_LOCK_PAGES;
            let last_erased = (leg_end.div_ceil(block) * block - 1 - plane * plane_size) / page;
            let last_erased = last_erased.min((plane_size / page).saturating_sub(1));
            let hit: Vec<u32> = (region(first_page)..=region(last_erased.max(last_page)))
                .filter(|region| bit_set(*region))
                .collect();
            if !hit.is_empty() {
                return Err(FlashError::Refused(format!(
                    "controller {eefc:#010x} reports lock regions {hit:?} locked and this image \
                     reaches them -- an erase or write there raises FLOCKE and does nothing. Its \
                     lock bits read {locked:#010x?}, {SAM4S_LOCK_PAGES} pages to a region."
                )));
            }
            legs.push(Sam4sLeg {
                eefc,
                plane_base,
                first_page,
                pages_in_plane: plane_size / page,
                bytes: (at - offset) as usize..(leg_end - offset) as usize,
            });
            at = leg_end;
        }
        Ok(legs)
    }
}

/// One controller's share of a dual-plane SAM4S image.
struct Sam4sLeg {
    /// Which EEFC user interface commands this share.
    eefc: u32,
    /// The window that controller's plane is mapped at -- where the latch buffer is filled.
    plane_base: u32,
    /// The first page NUMBER within that plane, which is not the page number within the image.
    first_page: u32,
    /// How many pages the plane holds, so the erase walk can be bounded inside it.
    pages_in_plane: u32,
    /// The bytes of the image this leg covers.
    bytes: std::ops::Range<usize>,
}

/// One controller's share of a SAM3X image.
struct Sam3xLeg {
    /// Which EEFC user interface commands this share.
    eefc: u32,
    /// The flash window that controller's plane is mapped at -- where the latch buffer is filled.
    plane_base: u32,
    /// The first page NUMBER within that plane, which is not the page number within the image.
    first_page: u32,
    /// The bytes of the image this leg covers.
    bytes: std::ops::Range<usize>,
}

impl<A: TargetAccess> FlashBackend for SamProbe<A> {
    fn mechanism(&self) -> &'static str {
        self.mechanism
    }

    fn flash_base(&self) -> u32 {
        self.family.flash_base()
    }

    /// The DSU device id, which is the reading that names Microchip's die.
    ///
    /// **AND THE PART CRATE ALREADY KNOWS WHICH ROUTINE DRIVES A GIVEN DIE**, so the refusal here
    /// consults [`SamDeviceId::flash_routine`] rather than restating a table. A part driven by
    /// ANOTHER routine in that crate is refused with that routine NAMED -- which is a different
    /// message from a part nobody has a datasheet for, and the two call for opposite next steps.
    ///
    /// **A CORTEX-M0-CLASS DP IDCODE IS ANSWERED BY PARTS FROM TWO VENDORS**, so the debug port's
    /// own id settles nothing here; the DSU's does.
    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        if self.family.identity_register() == crate::SamIdentity::Sam3xChipid {
            let cidr = self.target.read_word(SAM3X_CHIPID_CIDR)?;
            let Some(part) = sam3x_identify(cidr) else {
                return Err(FlashError::Refused(format!(
                    "CHIPID reports CIDR {cidr:#010x}, which is no SAM3X or SAM3A this tree knows \
                     -- refused rather than driven, because a flash routine pointed at an unknown \
                     die is the one case where guessing costs somebody else's board."
                )));
            };
            return Ok(PartIdentity { value: u64::from(cidr), what: part });
        }
        if let crate::SamIdentity::Sam4Chipid(families) = self.family.identity_register() {
            let cidr = self.target.read_word(SAM4_CHIPID_CIDR)?;
            let exid = self.target.read_word(SAM4_CHIPID_EXID)?;
            if !families.iter().any(|family| sam4_family_matches(cidr, family)) {
                return Err(FlashError::Refused(format!(
                    "CHIPID reports CIDR {cidr:#010x} / EXID {exid:#010x}, which is not the {} \
                     this route drives -- refused rather than driven, because a flash routine \
                     pointed at the wrong controller is the one case where guessing costs \
                     somebody else's board.",
                    self.family.controller()
                )));
            }
            return Ok(PartIdentity {
                value: u64::from(cidr),
                what: match sam4_identify(cidr, exid) {
                    Some(part) => part.part,
                    None => self.family.what(),
                },
            });
        }
        let id = self.target.sam_device_id()?;
        let drives = match self.family {
            crate::SamFamily::Samd21 => id.drives_samd21_nvmctrl(),
            crate::SamFamily::Same54 => id.drives_same54_nvmctrl(),
            crate::SamFamily::Sam4Eefc
            | crate::SamFamily::Sam4l
            | crate::SamFamily::Sam3x
            | crate::SamFamily::Sam4sDual => {
                unreachable!("handled above, by a different register")
            }
        };
        if !drives {
            return Err(FlashError::Refused(format!(
                "the DSU reports DID {:#010x} -- processor {:#x}, family {:#x}, series {:#x} -- \
                 which is not the {} this route drives{}.",
                id.raw,
                id.processor,
                id.family,
                id.series,
                self.family.controller(),
                match id.flash_routine() {
                    Some(other) => format!("; this part is driven by {other}"),
                    None => String::from(", and no routine in this tree claims it"),
                }
            )));
        }
        Ok(PartIdentity { value: u64::from(id.raw), what: self.family.what() })
    }

    /// Ask the part its geometry, then halt and erase what the image covers.
    ///
    /// **THE GEOMETRY COMES FROM THE PART.** `NVMCTRL_PARAM` reports the page count and page size,
    /// and the erase granule is derived from the page size rather than assumed -- a SAM D21 row is
    /// four pages of whatever size that part reports, not a constant.
    fn erase(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        if self.family == crate::SamFamily::Sam4Eefc {
            let descriptor = self.target.sam4s_flash_descriptor(SAM4E_EEFC)?;
            if descriptor.planes != 1 {
                return Err(FlashError::Refused(format!(
                    "this part reports {} flash planes and this route drives single-plane SAM4s \
                     only; a dual-plane part's controller is chosen by a GPNVM swap bit rather than \
                     by the address, and driving the wrong one programs the other plane",
                    descriptor.planes
                )));
            }
            let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
            let first_page = (image.base.saturating_sub(SAM4E_FLASH_BASE)) / descriptor.page_size;
            let pages = wanted.div_ceil(descriptor.page_size);
            let chunks = pages.div_ceil(SAM4S_ERASE_PAGES);
            let walk_end_page = first_page + chunks * SAM4S_ERASE_PAGES;
            let fitted_pages = descriptor.plane_bytes / descriptor.page_size;
            if walk_end_page > fitted_pages {
                return Err(FlashError::Refused(format!(
                    "erasing {wanted} bytes from page {first_page} walks to page {walk_end_page}, \
                     past the {fitted_pages} pages ({} KB) this plane reports",
                    descriptor.plane_bytes / 1024
                )));
            }
            if first_page % SAM4S_ERASE_PAGES != 0 {
                return Err(FlashError::Refused(format!(
                    "an EPA erase starts on a multiple of {SAM4S_ERASE_PAGES} pages and this image \
                     starts at page {first_page}"
                )));
            }
            self.target.halt()?;
            for chunk in 0..chunks {
                self.target
                    .sam4s_erase_pages8(SAM4E_EEFC, first_page + chunk * SAM4S_ERASE_PAGES)?;
            }
            return Ok(());
        }
        if self.family == crate::SamFamily::Sam4l {
            let params = self.target.sam4l_flash_parameters()?;
            let Some(flash_size) = params.flash_size else {
                return Err(FlashError::Refused(format!(
                    "FPR reads {:#010x}, whose FSZ is the reserved code -- the part is not saying \
                     how much flash it has, and a walk bounded by a guess is the thing this check \
                     exists to prevent",
                    params.raw
                )));
            };
            let pages = flash_size / params.page_size;
            let offset = image.base.saturating_sub(SAM4L_FLASH_BASE);
            if offset % params.page_size != 0 {
                return Err(FlashError::Refused(format!(
                    "this image starts {offset:#x} into the array and this part's page is {} \
                     bytes; a FLASHCALW erase and write are both whole pages",
                    params.page_size
                )));
            }
            let first_page = offset / params.page_size;
            let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
            let needed = wanted.div_ceil(params.page_size);
            let walk_end_page = first_page.saturating_add(needed);
            if walk_end_page > pages {
                return Err(FlashError::Refused(format!(
                    "erasing {wanted} bytes from page {first_page} walks to page {walk_end_page}, \
                     past the {pages} pages ({} KB) this part reports fitted",
                    flash_size / 1024
                )));
            }
            if self.target.sam4l_is_secure()? {
                return Err(FlashError::Refused(String::from(
                    "FSR.SECURITY is set: this part is in its protected state, which refuses debug \
                     access to flash. Nothing on this route can clear it -- that is the external \
                     chip-erase pin, and it takes the whole array with it.",
                )));
            }
            let locked = self.target.sam4l_lock_bits()?;
            if locked != 0 && pages != 0 {
                let region_of = |page: u32| (page * SAM4L_LOCK_REGIONS) / pages;
                let hit: Vec<u32> = (region_of(first_page)..=region_of(walk_end_page - 1))
                    .filter(|region| locked & (1 << region) != 0)
                    .collect();
                if !hit.is_empty() {
                    return Err(FlashError::Refused(format!(
                        "FSR reports lock regions {hit:?} locked, and this image covers them -- an \
                         erase or write there raises LOCKE and does nothing. The lock bits read \
                         {locked:#06x} across {SAM4L_LOCK_REGIONS} regions of {} pages each.",
                        pages / SAM4L_LOCK_REGIONS
                    )));
                }
            }
            self.target.halt()?;
            for page in first_page..walk_end_page {
                self.target.sam4l_erase_page(page)?;
            }
            return Ok(());
        }
        if self.family == crate::SamFamily::Sam4sDual {
            let legs = self.sam4s_dual_plan(image)?;
            self.target.halt()?;
            for leg in legs {
                let bytes = u32::try_from(leg.bytes.len()).unwrap_or(u32::MAX);
                let chunks = bytes.div_ceil(SAM4S_PAGE as u32 * SAM4S_ERASE_PAGES);
                for chunk in 0..chunks {
                    let page = leg.first_page + chunk * SAM4S_ERASE_PAGES;
                    debug_assert!(page < leg.pages_in_plane, "the plan bounds the walk to a plane");
                    self.target.sam4s_erase_pages8(leg.eefc, page)?;
                }
            }
            return Ok(());
        }
        if self.family == crate::SamFamily::Sam3x {
            self.sam3x_plan(image)?;
            self.target.halt()?;
            return Ok(());
        }
        let geometry = self.target.sam_flash_geometry()?;
        let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        let granule = match self.family {
            crate::SamFamily::Samd21 => geometry.samd21_row_bytes(),
            crate::SamFamily::Same54 => SAME54_BLOCK,
            crate::SamFamily::Sam4Eefc
            | crate::SamFamily::Sam4l
            | crate::SamFamily::Sam3x
            | crate::SamFamily::Sam4sDual => {
                unreachable!("handled above: those controllers walk pages, not addresses")
            }
        };
        let granules = wanted.div_ceil(granule);
        let walk_end = image.base.saturating_add(granules.saturating_mul(granule));
        let array_end = self.flash_base().saturating_add(geometry.flash_bytes());
        if walk_end > array_end {
            return Err(FlashError::Refused(format!(
                "erasing {wanted} bytes from {:#010x} walks to {walk_end:#010x}, past the {} KB \
                 this part reports fitted, whose array ends at {array_end:#010x}",
                image.base,
                geometry.flash_bytes() / 1024
            )));
        }
        self.target.halt()?;
        for granule_index in 0..granules {
            let at = image.base + granule_index * granule;
            match self.family {
                crate::SamFamily::Samd21 => self.target.erase_flash_row(at)?,
                crate::SamFamily::Same54 => self.target.erase_flash_block(at)?,
                crate::SamFamily::Sam4Eefc
                | crate::SamFamily::Sam4l
                | crate::SamFamily::Sam3x
                | crate::SamFamily::Sam4sDual => {
                    unreachable!("handled above: those controllers walk pages, not addresses")
                }
            }
        }
        Ok(())
    }

    /// Program the image. Both NVMCTRL families take words at an address.
    ///
    /// **A SHORT TAIL IS PADDED WITH `0xFF`, WHICH IS THIS FAMILY'S ERASED VALUE**, so the padding is
    /// not information written into a cell that had none.
    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let mut padded = image.bytes.to_vec();
        while padded.len() % 4 != 0 {
            padded.push(0xff);
        }
        let words: Vec<u32> = padded
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        match self.family {
            crate::SamFamily::Samd21 => {
                Samd21Flash::write_flash(&mut self.target, image.base, &words)?
            }
            crate::SamFamily::Same54 => {
                Same54Flash::write_flash(&mut self.target, image.base, &words)?
            }
            crate::SamFamily::Sam4Eefc => {
                let descriptor = self.target.sam4s_flash_descriptor(SAM4E_EEFC)?;
                let first_page =
                    (image.base.saturating_sub(SAM4E_FLASH_BASE)) / descriptor.page_size;
                self.target.sam4s_write_flash(
                    SAM4E_EEFC,
                    SAM4E_FLASH_BASE,
                    first_page,
                    &words,
                )?
            }
            crate::SamFamily::Sam4sDual => {
                for leg in self.sam4s_dual_plan(image)? {
                    let start = leg.bytes.start / 4;
                    let end = leg.bytes.end.div_ceil(4);
                    self.target.sam4s_write_flash(
                        leg.eefc,
                        leg.plane_base,
                        leg.first_page,
                        &words[start..end],
                    )?;
                }
            }
            crate::SamFamily::Sam3x => {
                for leg in self.sam3x_plan(image)? {
                    let start = leg.bytes.start / 4;
                    let end = leg.bytes.end.div_ceil(4);
                    self.target.sam3x_write_flash(
                        leg.eefc,
                        leg.plane_base,
                        leg.first_page,
                        &words[start..end],
                    )?;
                }
            }
            crate::SamFamily::Sam4l => {
                let params = self.target.sam4l_flash_parameters()?;
                let per_page = (params.page_size / 4) as usize;
                let first_page =
                    (image.base.saturating_sub(SAM4L_FLASH_BASE)) / params.page_size;
                for (index, chunk) in words.chunks(per_page).enumerate() {
                    let mut page = chunk.to_vec();
                    page.resize(per_page, 0xffff_ffff);
                    let index = u32::try_from(index).unwrap_or(u32::MAX);
                    self.target.sam4l_write_page(
                        first_page + index,
                        params.page_size,
                        &page,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Every byte, read back over the same wire that wrote them.
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
