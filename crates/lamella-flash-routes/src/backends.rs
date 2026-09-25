//! The flashing backends, implemented against the contract.

use lamella_cmsis_dap_nrf::Nrf51Flash;
use lamella_cmsis_dap_sam::{
    SAM3X_CHIPID_CIDR, SAM3X_EEFC0, SAM3X_EEFC1, SAM3X_FLASH0_BASE, SAM3X_FLASH1_BASE,
    SAM3X_GPNVM_PLANE_SWAP, SAM3X_LOCK_PAGES, SAM3X_PAGE, SAM3X_PLANE_SIZE, SAM4E_EEFC,
    SAM4E_FLASH_BASE, SAM4L_FLASH_BASE, SAM4L_LOCK_REGIONS, SAM4S_EEFC0, SAM4S_EEFC1,
    SAM4S_ERASE_PAGES, SAM4S_FLASH0_BASE, SAM4S_FLASH1_BASE, SAM4S_GPNVM_PLANE_SWAP,
    SAM4S_LOCK_PAGES, SAM4S_PAGE, SAM4_CHIPID_CIDR, SAM4_CHIPID_EXID, SAMD21_LOCK_REGIONS,
    SAME54_BLOCK, Sam3xFlash, Sam4lFlash, Sam4sFlash, Samd21Flash, Samd21UserRow,
    Samd21UserRowAccess, SamIdentify, Same54Flash, sam3x_identify, sam4_family_matches,
    sam4_identify, samd21_bootprot_bytes, samd21_eeprom_bytes,
};
use lamella_cmsis_dap_stm32::{
    STM32C0_DBGMCU_IDCODE, STM32C0_DOUBLE_WORD, STM32C0_ERASED_VALUE, STM32C0_FLASH_BASE,
    STM32C0_FLASH_SIZE_REG, STM32C0_PAGE, STM32C0_PARTS, STM32F7_DBG_IWDG_STOP,
    STM32F7_DBG_WWDG_STOP, STM32F7_DBGMCU_APB1_FZ, STM32F7_DBGMCU_CR,
    STM32F7_DBGMCU_CR_LOW_POWER_DEBUG, STM32F7_DBGMCU_IDCODE, STM32F7_FLASH_BASE,
    STM32F7_FLASH_SIZE_REG, STM32F7_PARTS, STM32H7_BANK2_BASE, STM32H7_DBGMCU_IDC,
    STM32H7_FLASH_BASE, STM32H7_FLASH_SIZE_REG, STM32H7_FLASH_WORD, STM32H7_PARTS, STM32H7_SECTOR,
    STM32L0_DBGMCU_IDCODE, STM32L0_ERASED_WORD, STM32L0_FLASH_BASE, STM32L0_FLASH_SIZE_REG,
    STM32L0_PAGE, STM32L0_PARTS, STM32L4_DBGMCU_IDCODE, STM32L4_DOUBLE_WORD, STM32L4_ERASED_WORD,
    STM32L4_FLASH_BASE, STM32L4_FLASH_SIZE_REG, STM32L4_PAGE, STM32L4_PARTS, STM32U5_DBGMCU_IDCODE,
    STM32U5_FLASH_BASE, STM32U5_FLASH_SIZE_REG, STM32U5_PAGE, STM32U5_PARTS, STM32U5_QUAD_WORD,
    Stm32C0Flash, Stm32F4Flash, Stm32H7Flash, Stm32L0Flash, Stm32L4Flash, Stm32U5Flash,
    stm32_dev_id, stm32_flash_size_bytes, stm32f7_read_sector_sizes,
};
use lamella_flash_backend::{FlashBackend, FlashError, Image, PartIdentity};
use lamella_probe_core::{TargetAccess, TargetAccessExt};

/// Leaves a written part running its image: every hardware breakpoint removed, then a reset to run.
///
/// **The breakpoints come off first because nothing else takes them off.** A comparator a debug
/// session armed outlives a system reset -- `FP_CTRL.ENABLE` "resets to zero on a Cold reset"
/// (Armv8-M Architecture Reference Manual, DDI 0553B.y, D1.2, `FP_CTRL`) -- and a session that ends
/// without handing the part back leaves its comparators armed. Halting debug is still on after
/// the reset, so the new image halts at the first of those addresses its own code reaches, with no
/// debugger attached to resume it.
fn leave_running<A: TargetAccess>(target: &mut A) -> Result<(), FlashError> {
    target.set_breakpoints(&[])?;
    target.reset_and_run()?;
    Ok(())
}

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
        leave_running(&mut self.target)?;
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
        leave_running(&mut self.target)?;
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
        leave_running(&mut self.target)?;
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
                        "no board is in its bootloader. Hold BOOTSEL while plugging the board in \
                         (or press RESET
with BOOTSEL held), and it will appear as a drive."
                            .to_owned(),
                    ));
                }
                [only] => only.volume.clone(),
                several => {
                    let list: Vec<&str> =
                        several.iter().map(|found| found.volume.as_str()).collect();
                    return Err(FlashError::Refused(format!(
                        "{} boards are in their bootloader and nothing on a volume tells them \
                         apart: {}\n\n\
                         Name one with --volume <name>. Their labels and their INFO_UF2.TXT files \
                         are identical, so this will not guess -- the wrong choice writes your \
                         program to the wrong board.",
                        several.len(),
                        list.join(", ")
                    )));
                }
            },
        };
        self.chosen = Some(std::path::PathBuf::from(volume));
        Ok(PartIdentity {
            value: u64::from(self.family),
            what: "a UF2 family, which every board of that family shares and which settles \
                   nothing about WHICH board is attached",
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

/// How a family's flash divides into the units one erase operation takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EraseMap {
    /// Every unit is this many bytes and the controller finds the one holding an address: a page on
    /// the L0, C0, L4 and U5, a sector on the H7.
    Stride(u32),
    /// Sectors of unequal size, numbered in address order from the base of the array, whose sizes the
    /// part itself decides. The walk reads the map from the part when it erases, and commands each
    /// sector by its number.
    PartSectors,
}

/// One erase unit a walk reaches: where it starts, how many bytes it spans, and its number in the map.
///
/// A stride family's controller is handed `start` and finds its own unit; a
/// [`EraseMap::PartSectors`] controller is handed `index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Granule {
    start: u32,
    size: u32,
    index: u32,
}

/// The register that stops a family's watchdogs while its core is halted, and the bits that do it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchdogFreeze {
    /// The debug freeze register.
    pub register: u32,
    /// The bits that stop the watchdogs, set on top of whatever the register already holds.
    pub bits: u32,
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
    /// How the array divides into the units one erase takes.
    pub erase: EraseMap,
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
    /// **AN STM32F7 WHOSE FIRMWARE SLEEPS FAILS IN THE SILENT DIRECTION: EVERY READ SUCCEEDS.** With
    /// `DBG_SLEEP` clear the part turns off a clock its debugger connection needs whenever the core
    /// waits for an interrupt, and a probe can then answer every memory read with one word and
    /// report each read as a success. A part held in reset is not asleep, so the F7 plan attaches
    /// this way too and sets [`low_power_debug`](Self::low_power_debug) while the core is held.
    ///
    /// False for the other families here, because the plain path is what those families were
    /// driven with and an attach is not the place to change a proven route's behavior
    /// speculatively.
    pub attach_under_reset: bool,
    /// The register and bits an attach under reset sets while the core is held, so a debugger
    /// connection keeps its clocks when the part's firmware puts the core to sleep. `None` sets
    /// nothing.
    ///
    /// Only an attach under reset writes them: a part whose core sleeps is reached while it is
    /// held in reset, and a plain attach does not hold it.
    pub low_power_debug: Option<lamella_stlink::LowPowerDebug>,
    /// The watchdogs to stop for the length of a write: set before the core is halted, and put back
    /// as found before the part is released. `None` means this plan does not stop them, not that the
    /// part has no watchdog.
    pub watchdog_freeze: Option<WatchdogFreeze>,
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
    erase: EraseMap::Stride(STM32L0_PAGE),
    erased_word: STM32L0_ERASED_WORD,
    program_align: 4,
    banks: &[STM32L0_FLASH_BASE],
    attach_under_reset: false,
    low_power_debug: None,
    watchdog_freeze: None,
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
    erase: EraseMap::Stride(STM32C0_PAGE),
    erased_word: STM32C0_ERASED_VALUE,
    program_align: STM32C0_DOUBLE_WORD as u32,
    banks: &[STM32C0_FLASH_BASE],
    attach_under_reset: false,
    low_power_debug: None,
    watchdog_freeze: None,
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
    erase: EraseMap::Stride(STM32L4_PAGE),
    erased_word: STM32L4_ERASED_WORD,
    program_align: STM32L4_DOUBLE_WORD as u32,
    banks: &[STM32L4_FLASH_BASE],
    attach_under_reset: false,
    low_power_debug: None,
    watchdog_freeze: None,
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
    erase: EraseMap::Stride(STM32H7_SECTOR),
    erased_word: 0xffff_ffff,
    program_align: STM32H7_FLASH_WORD as u32,
    banks: &[STM32H7_FLASH_BASE, STM32H7_BANK2_BASE],
    attach_under_reset: true,
    low_power_debug: None,
    watchdog_freeze: None,
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
    erase: EraseMap::Stride(STM32U5_PAGE),
    erased_word: 0xffff_ffff,
    program_align: STM32U5_QUAD_WORD as u32,
    banks: &[STM32U5_FLASH_BASE],
    attach_under_reset: false,
    low_power_debug: None,
    watchdog_freeze: None,
    unit: "quad-words",
    manual: "RM0456 75.12.4",
};

/// The STM32F7: sectors of 32, 128 and 256 KB in one array behind one lock, programmed a 32-bit word
/// at a time through the F4 controller primitives, whose register block, keys and `FLASH_CR` bit
/// positions RM0385 gives the F7 unchanged.
const F7_PLAN: StPlan = StPlan {
    family: crate::StFamily::F7,
    flash_base: STM32F7_FLASH_BASE,
    size_register: STM32F7_FLASH_SIZE_REG,
    id_register: STM32F7_DBGMCU_IDCODE,
    parts: STM32F7_PARTS,
    erase: EraseMap::PartSectors,
    erased_word: 0xffff_ffff,
    program_align: 4,
    banks: &[STM32F7_FLASH_BASE],
    attach_under_reset: true,
    low_power_debug: Some(lamella_stlink::LowPowerDebug {
        register: STM32F7_DBGMCU_CR,
        bits: STM32F7_DBGMCU_CR_LOW_POWER_DEBUG,
    }),
    watchdog_freeze: Some(WatchdogFreeze {
        register: STM32F7_DBGMCU_APB1_FZ,
        bits: STM32F7_DBG_IWDG_STOP | STM32F7_DBG_WWDG_STOP,
    }),
    unit: "words",
    manual: "RM0385/RM0410",
};

impl crate::StFamily {
    /// The numbers this family's flash write needs.
    ///
    /// **A `match` WITH NO DEFAULT ARM, DELIBERATELY.** Adding a variant to [`crate::StFamily`]
    /// without a plan is then a compile error rather than a route that resolves at run time to
    /// another family's register addresses.
    pub fn plan(self) -> &'static StPlan {
        match self {
            crate::StFamily::L0 => &L0_PLAN,
            crate::StFamily::C0 => &C0_PLAN,
            crate::StFamily::L4 => &L4_PLAN,
            crate::StFamily::H7 => &H7_PLAN,
            crate::StFamily::U5 => &U5_PLAN,
            crate::StFamily::F7 => &F7_PLAN,
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
    /// The watchdog freeze register as the write found it, put back before the part is released.
    watchdogs_found: Option<u32>,
}

impl<A: TargetAccess> StProbe<A> {
    /// A backend for the family `plan` describes, reached through `target`.
    pub fn new(target: A, plan: &'static StPlan) -> Self {
        StProbe {
            target,
            plan,
            watchdogs_found: None,
        }
    }

    /// The probe this backend was built over, handed back once a write is done, for a caller that
    /// goes on to use the part.
    pub fn into_target(self) -> A {
        self.target
    }

    /// Takes the lock covering `at`. Idempotent on every family here, and it has to be: each of them
    /// locks its control register until the next system reset on a wrong key sequence, so each
    /// primitive reads the lock bit before writing a key.
    fn unlock(&mut self, at: u32) -> Result<(), FlashError> {
        match self.plan.family {
            crate::StFamily::L0 => self.target.l0_unlock_flash()?,
            crate::StFamily::C0 => self.target.c0_unlock_flash()?,
            crate::StFamily::L4 => self.target.l4_unlock_flash()?,
            crate::StFamily::H7 => self.target.h7_unlock_flash(at)?,
            crate::StFamily::U5 => self.target.u5_unlock_flash()?,
            crate::StFamily::F7 => Stm32F4Flash::unlock_flash(&mut self.target)?,
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
            crate::StFamily::F7 => Stm32F4Flash::lock_flash(&mut self.target)?,
        }
        Ok(())
    }

    /// Erases one granule the walk reached.
    fn erase_granule(&mut self, granule: Granule) -> Result<(), FlashError> {
        let at = granule.start;
        match self.plan.family {
            crate::StFamily::L0 => self.target.l0_erase_page(at)?,
            crate::StFamily::C0 => self.target.c0_erase_page(at)?,
            crate::StFamily::L4 => self.target.l4_erase_page(at)?,
            crate::StFamily::H7 => self.target.h7_erase_sector(at)?,
            crate::StFamily::U5 => self.target.u5_erase_page(at)?,
            crate::StFamily::F7 => Stm32F4Flash::erase_sector(&mut self.target, granule.index)?,
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
            crate::StFamily::F7 => {
                if data.len() % 4 != 0 {
                    return Err(FlashError::Refused(format!(
                        "{} bytes is not a whole number of the 32-bit words this controller programs",
                        data.len()
                    )));
                }
                let words: Vec<u32> = data
                    .chunks_exact(4)
                    .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
                    .collect();
                Stm32F4Flash::program_words(&mut self.target, at, &words)?
            }
        }
        Ok(())
    }

    /// Stops the plan's watchdogs while the core is halted, keeping the register's value as found.
    fn freeze_watchdogs(&mut self) -> Result<(), FlashError> {
        let Some(freeze) = self.plan.watchdog_freeze else {
            return Ok(());
        };
        let found = self.target.read_word(freeze.register)?;
        self.watchdogs_found.get_or_insert(found);
        self.target.write_word(freeze.register, found | freeze.bits)?;
        Ok(())
    }

    /// The erase units an image at `base` of `len` bytes reaches, in address order: for a stride
    /// family, units of one size from `base` with the last rounded up; for a mapped family, every
    /// sector of the part's map the image overlaps, the one it starts inside included.
    fn granules_covering(&mut self, base: u32, len: u32) -> Result<Vec<Granule>, FlashError> {
        match self.plan.erase {
            EraseMap::Stride(size) => Ok(stride_granules(base, len, size)),
            EraseMap::PartSectors => {
                let sizes = self.sector_sizes()?;
                if base < self.plan.flash_base {
                    return Err(FlashError::Refused(format!(
                        "this image is based at {base:#010x}, below the {:#010x} where this part's \
                         sectors begin",
                        self.plan.flash_base
                    )));
                }
                let end = base.saturating_add(len);
                let mut start = self.plan.flash_base;
                let mut reached = Vec::new();
                for (index, size) in sizes.iter().enumerate() {
                    let size = u32::try_from(*size).unwrap_or(u32::MAX);
                    let next = start.saturating_add(size);
                    if len > 0 && start < end && base < next {
                        reached.push(Granule {
                            start,
                            size,
                            index: u32::try_from(index).unwrap_or(u32::MAX),
                        });
                    }
                    start = next;
                }
                if len > 0 && start < end {
                    return Err(FlashError::Refused(format!(
                        "this part's sector map ends at {start:#010x} and the image runs to {end:#010x}"
                    )));
                }
                Ok(reached)
            }
        }
    }

    /// The sector map a [`EraseMap::PartSectors`] family's part is using, read from the part.
    fn sector_sizes(&mut self) -> Result<&'static [usize], FlashError> {
        match self.plan.family {
            crate::StFamily::F7 => Ok(stm32f7_read_sector_sizes(&mut self.target)?),
            crate::StFamily::L0
            | crate::StFamily::C0
            | crate::StFamily::L4
            | crate::StFamily::H7
            | crate::StFamily::U5 => Err(FlashError::Refused(format!(
                "{} erases by a stride and has no sector map to read",
                self.plan.family.name()
            ))),
        }
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
        let granules = self.granules_covering(image.base, wanted)?;
        let walk_end = walk_within_fitted_flash(image, &granules, self.plan, fitted)?;
        self.freeze_watchdogs()?;
        self.target.halt()?;
        let banks = self.banks_covering(image.base, walk_end.saturating_sub(image.base));
        for bank in &banks {
            self.unlock(*bank)?;
        }
        for granule in granules {
            self.erase_granule(granule)?;
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
    /// on the other family -- see [`padded_to_program_granule`].
    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        let banks = self.banks_covering(image.base, wanted);
        for bank in &banks {
            self.unlock(*bank)?;
        }
        let padded = padded_to_program_granule(image.bytes, self.plan);
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
        if let (Some(freeze), Some(found)) = (self.plan.watchdog_freeze, self.watchdogs_found.take()) {
            self.target.write_word(freeze.register, found)?;
        }
        leave_running(&mut self.target)?;
        Ok(())
    }
}

/// The erase units a stride of `size` bytes gives an image at `base` of `len` bytes, in address order,
/// the last one rounded up to a whole unit.
fn stride_granules(base: u32, len: u32, size: u32) -> Vec<Granule> {
    (0..len.div_ceil(size))
        .map(|index| Granule {
            start: base.saturating_add(index.saturating_mul(size)),
            size,
            index,
        })
        .collect()
}

/// Where the erase walk `granules` ends, or the refusal when that is past the flash the part reports
/// fitted -- `fitted` bytes from the plan's base.
///
/// **THE BOUND IS ON WHERE THE WALK REACHES, NOT ON THE IMAGE's LENGTH.** A walk starts at the image's
/// base and rounds its last unit up, so a bound on the image's length covers the walk only while the
/// image starts at the flash base -- an equality `lamella_flash_backend::flash` enforces from another
/// crate, which a backend does not get to depend on.
fn walk_within_fitted_flash(
    image: &Image<'_>,
    granules: &[Granule],
    plan: &StPlan,
    fitted: u32,
) -> Result<u32, FlashError> {
    let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
    let walk_end = granules
        .last()
        .map_or(image.base, |last| last.start.saturating_add(last.size));
    let array_end = plan.flash_base.saturating_add(fitted);
    if walk_end > array_end {
        return Err(FlashError::Refused(format!(
            "erasing {wanted} bytes from {:#010x} walks to {walk_end:#010x}, past the {} KB \
             this part reports fitted, whose array ends at {array_end:#010x}",
            image.base,
            fitted / 1024
        )));
    }
    Ok(walk_end)
}

/// `bytes` padded to a whole number of `plan`'s program granules with the byte an erased cell of that
/// family already holds.
///
/// **SO PADDING IS NEVER A WRITE.** On the L0 that means a zero tail is skipped by the programmer and
/// those cells stay erased; on a ones-erasing part the same rule pads with `0xFF` and writes the flash
/// word once. The same line would be a defect on either part with the other one's value hard-coded,
/// which is why it is derived from the plan.
fn padded_to_program_granule(bytes: &[u8], plan: &StPlan) -> Vec<u8> {
    let filler = (plan.erased_word & 0xff) as u8;
    let mut padded = bytes.to_vec();
    while padded.len() % plan.program_align as usize != 0 {
        padded.push(filler);
    }
    padded
}

/// An STM32 written through its system bootloader's USB DFU interface, by the DfuSe commands of
/// AN3156, with no probe attached.
///
/// **THE BOOTLOADER PROGRAMS THE FLASH, SO THE PLAN SUPPLIES ADDRESSES AND NOTHING ELSE.** Unlocking a
/// bank, the flash word and the controller's error flags are the bootloader's to handle. What the
/// host still owns is where the array starts, the units it erases in, where the second bank begins,
/// and what a padded tail holds -- read from the same plan [`StProbe`] drives.
///
/// The pipe is opened, and the interface's transfer size read from its functional descriptor, by the
/// caller. The first thing this does is bring the bootloader to idle and read its ID, which touches no
/// flash.
pub struct StDfu<P: crate::dfu::ControlPipe> {
    dfu: crate::dfu::DfuSe<P>,
    plan: &'static StPlan,
    bootloader: &'static crate::dfu::SystemBootloader,
    /// The bootloader ID [`identify`](FlashBackend::identify) read, which decides whether an erase in
    /// the second bank needs the wait AN2606 prescribes for it.
    version: Option<u8>,
}

impl<P: crate::dfu::ControlPipe> StDfu<P> {
    /// A backend for the family `plan` describes, whose system bootloader `bootloader` describes,
    /// reached through `dfu`.
    pub fn new(
        dfu: crate::dfu::DfuSe<P>,
        plan: &'static StPlan,
        bootloader: &'static crate::dfu::SystemBootloader,
    ) -> Self {
        StDfu { dfu, plan, bootloader, version: None }
    }

    /// Gives the protocol back, with its pipe.
    pub fn into_dfu(self) -> crate::dfu::DfuSe<P> {
        self.dfu
    }

    /// `length` bytes from `address`, through the bootloader's Read memory command.
    fn read(&mut self, address: u32, length: usize) -> Result<Vec<u8>, FlashError> {
        self.dfu.read(address, length).map_err(dfu_refusal)
    }
}

/// A DFU exchange that stopped, as a flash backend reports it: in the protocol's own words.
fn dfu_refusal(why: crate::dfu::DfuError) -> FlashError {
    FlashError::Refused(why.to_string())
}

impl<P: crate::dfu::ControlPipe> FlashBackend for StDfu<P> {
    fn mechanism(&self) -> &'static str {
        "the part's system bootloader, over USB DFU"
    }

    fn flash_base(&self) -> u32 {
        self.plan.flash_base
    }

    /// The bootloader's ID byte, read from where AN2606 Rev 70 keeps it for the series (4.2 and
    /// Table 3), refused unless the series' version table lists it.
    ///
    /// **IT NAMES A BOOTLOADER VERSION, AND THROUGH IT A SERIES -- NEVER A BOARD.** Every part of the
    /// series carrying that version answers the same byte. What it rules out is a bootloader AN2606
    /// lists for no part of the series.
    fn identify(&mut self) -> Result<PartIdentity, FlashError> {
        self.dfu.to_idle().map_err(dfu_refusal)?;
        let id = self.read(self.bootloader.id_address, 2)?[0];
        if !self.bootloader.versions.iter().any(|(listed, _)| *listed == id) {
            let listed: Vec<String> = self
                .bootloader
                .versions
                .iter()
                .map(|(listed, name)| format!("{listed:#04x} ({name})"))
                .collect();
            return Err(FlashError::Refused(format!(
                "the bootloader ID at {:#010x} reads {id:#04x}, which AN2606 Rev 70 lists for no {} \
                 system bootloader; it lists {}.",
                self.bootloader.id_address,
                self.bootloader.series,
                listed.join(", ")
            )));
        }
        self.version = Some(id);
        Ok(PartIdentity {
            value: u64::from(id),
            what: self.bootloader.settles,
        })
    }

    /// Ask the part how much flash is fitted, then erase the units the image covers, one DfuSe erase
    /// command each.
    ///
    /// **THE SIZE COMES FROM THE PART**, read through the bootloader from the plan's flash-size
    /// register, and it bounds where the walk reaches exactly as it does for [`StProbe`]. After each
    /// erase in the second bank, a bootloader version that AN2606 lists as answering before that
    /// erase has finished is given the wait the series' facts state before the next command.
    fn erase(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let EraseMap::Stride(size) = self.plan.erase else {
            return Err(FlashError::Refused(format!(
                "{} erases by a sector map read from the part, and this route reads none",
                self.plan.family.name()
            )));
        };
        let register = self.read(self.plan.size_register, 2)?;
        let kb = u32::from(u16::from_le_bytes([register[0], register[1]]));
        if kb == 0 || kb == 0xffff {
            return Err(FlashError::Refused(format!(
                "the flash-size register at {:#010x} reads blank through the bootloader",
                self.plan.size_register
            )));
        }
        let wanted = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        let granules = stride_granules(image.base, wanted, size);
        walk_within_fitted_flash(image, &granules, self.plan, kb * 1024)?;
        let second_bank = self.plan.banks.get(1).copied();
        for granule in granules {
            self.dfu.erase_page(granule.start).map_err(dfu_refusal)?;
            if let (Some((version, milliseconds)), Some(bank)) =
                (self.bootloader.early_bank2_erase, second_bank)
                && self.version == Some(version)
                && granule.start >= bank
            {
                self.dfu.wait(milliseconds);
            }
        }
        Ok(())
    }

    /// Program the image through the bootloader, its tail padded as [`padded_to_program_granule`]
    /// pads it.
    fn program(&mut self, image: &Image<'_>) -> Result<(), FlashError> {
        let padded = padded_to_program_granule(image.bytes, self.plan);
        self.dfu.write(image.base, &padded).map_err(dfu_refusal)
    }

    /// Every byte, read back through the bootloader's Read memory command.
    ///
    /// A bootloader that answers an upload has a read-back, so this must use it.
    fn read_back(&mut self, image: &Image<'_>) -> Option<Result<Vec<u8>, FlashError>> {
        let programmed = image.bytes.len().next_multiple_of(self.plan.program_align as usize);
        Some(self.read(image.base, programmed).map(|mut bytes| {
            bytes.truncate(image.bytes.len());
            bytes
        }))
    }

    /// Leave DFU mode and start the image from the flash base.
    ///
    /// **A BOOTLOADER THAT MANIFESTS HAS NOT NECESSARILY STARTED THE IMAGE** -- see
    /// [`DfuSe::leave`](crate::dfu::DfuSe::leave) -- so a board that does nothing afterwards is reset
    /// by hand.
    fn finish(&mut self) -> Result<(), FlashError> {
        self.dfu.leave(self.plan.flash_base).map_err(dfu_refusal)
    }
}

/// A Microchip SAM reached through its on-board EDBG, driven by the part's own flash controller.
///
/// # Why this is NOT the ST backend with different constants
///
/// [`StProbe`] shares one SEQUENCE across six families because they genuinely have one: ask the
/// size, halt, unlock, walk the granules, lock, program, read back, reset. **The SAM controllers do
/// not.** A SAM3X has no page erase at all -- only erase-all -- so it has no granule walk to share;
/// a SAM4L must invalidate its flash cache after a write or a correct write reads back as all ones
/// forever; an EEFC addresses by controller base and PAGE NUMBER rather than by address, and carries
/// lock bits that may have to be cleared first.
///
/// **So what is shared here is the CONTRACT, not the sequence**, and this type is honest about that:
/// [`read_back`](FlashBackend::read_back), [`finish`](FlashBackend::finish) and the identify-refusal
/// shape are written once, and erase and program dispatch to arms that are allowed to differ in
/// shape rather than only in constants. Forcing them into one walk is the mistake already
/// made once by comparing two mechanisms and getting the comparison wrong.
///
/// # The connect happens before construction
///
/// As with every backend here: the probe is opened, brought into SWD and given memory access by the
/// caller. The first thing this does is a read that touches nothing.
///
/// # Behind a bootloader
///
/// A write [`placed`](Self::placed) behind a bootloader starts where the bootloader ends and leaves
/// its rows alone. Before it erases anything it reads the vector table where the bootloader
/// belongs, and refuses when the part does not hold one there: the board's facts state what its
/// product ships, and a unit whose bootloader was overwritten is still that board.
pub struct SamProbe<A: TargetAccess> {
    target: A,
    family: crate::SamFamily,
    mechanism: &'static str,
    /// Where this write puts the image: the family's flash base, or behind a kept bootloader.
    base: u32,
    /// The bootloader this write keeps in front of the image, which the part must hold.
    kept: Option<crate::placement::ShippedBootloader>,
}

impl<A: TargetAccess> SamProbe<A> {
    /// A backend for `family`, reached through `target`, describing itself as `mechanism`, that
    /// writes an image from the start of the family's flash.
    ///
    /// **THE MECHANISM IS THE ROUTE'S FACT AND NOT THE FAMILY'S**, which is why it is passed in
    /// rather than derived here. The same controller is reached through a debugger soldered to the
    /// board on an Xplained kit and through a probe the reader supplied on an Arduino Due, and the
    /// sentence a person reads afterwards is about which of those happened.
    pub fn new(target: A, family: crate::SamFamily, mechanism: &'static str) -> Self {
        SamProbe { target, family, mechanism, base: family.flash_base(), kept: None }
    }

    /// The same backend, writing where `placement` puts the image.
    ///
    /// The image handed to [`lamella_flash_backend::flash`] must be built from the same
    /// `placement`, which is what the contract's base check compares.
    #[must_use]
    pub fn placed(mut self, placement: &crate::placement::Placement) -> Self {
        self.base = placement.base();
        self.kept = placement.kept().cloned();
        self
    }

    /// Refuse unless the part holds, where this write keeps a bootloader, a vector table whose
    /// reset vector starts code inside that bootloader.
    ///
    /// Word 0 of a Cortex-M vector table is the initial stack pointer and word 1 the reset vector,
    /// whose bit 0 is set because the core runs Thumb code. An erased word 0, a reset vector
    /// without that bit, and one that starts code outside the bootloader's span are each refused,
    /// and nothing has been erased when they are.
    fn refuse_unless_the_bootloader_is_there(
        &mut self,
        kept: &crate::placement::ShippedBootloader,
    ) -> Result<(), FlashError> {
        let table = self.target.read_words(kept.base, 2)?;
        let (stack, reset) = (table[0], table[1]);
        let span = format!(
            "{:#010x}-{:#010x}",
            kept.base,
            kept.end().saturating_sub(1)
        );
        let found = if stack == u32::MAX {
            format!("the flash at {:#010x} is erased, so this part holds no bootloader", kept.base)
        } else if reset & 1 == 0 {
            format!(
                "the vector table at {:#010x} starts {stack:#010x} {reset:#010x}, and a reset \
                 vector without bit 0 set is not one a Cortex-M core takes, so that is not a \
                 bootloader",
                kept.base
            )
        } else if !(kept.base..kept.end()).contains(&(reset & !1)) {
            format!(
                "the vector table at {:#010x} starts {stack:#010x} {reset:#010x}, whose reset \
                 vector starts code at {:#010x}, outside the bootloader's {} bytes: that is an \
                 image that runs from {:#010x} on its own",
                kept.base,
                reset & !1,
                kept.size,
                kept.base
            )
        } else {
            return Ok(());
        };
        Err(FlashError::Refused(format!(
            "this write keeps the {} at {span} and writes the image behind it, and {found}. \
             Nothing was erased.\n\n\
             To keep a bootloader, write one first from its own file with --replace-bootloader; \
             none is\nincluded here. To run without one, write an image linked to start at \
             {:#010x} with\n--replace-bootloader.",
            kept.name, kept.base
        )))
    }

    /// Refuse an image whose erase walk, `image.base` up to `walk_end`, reaches a row a SAM D21 will
    /// not erase: one inside the rows BOOTPROT protects, or one in a locked region.
    ///
    /// An erase in either is not performed: the controller sets STATUS.LOCKE and the row keeps its
    /// bytes (DS40001882D 22.6.4.5). A write into one would fail only at its read-back, as a
    /// mismatch that names neither the cause nor the remedy.
    ///
    /// BOOTPROT's rows are a boot loader section from the start of the array, which the part
    /// write-protects (22.6.2, 22.6.5 and Table 22-2). The locks are checked by
    /// [`Self::refuse_locked_regions`].
    ///
    /// Only a part whose DSU names a SAM D21 is checked. The SAM D10 and D11 share this controller,
    /// and their user rows are left to their own datasheets.
    fn refuse_protected_rows(
        &mut self,
        image: &Image<'_>,
        walk_end: u32,
        flash_bytes: u32,
    ) -> Result<(), FlashError> {
        if !self.target.sam_device_id()?.has_samd21_user_row() {
            return Ok(());
        }
        let row = self.target.read_samd21_user_row()?;
        let bootprot = row.bootprot();
        let start = self.family.flash_base();
        let protected_end = start.saturating_add(samd21_bootprot_bytes(bootprot));
        if image.base < protected_end {
            return Err(FlashError::Refused(format!(
                "this part's BOOTPROT is {bootprot}, which write-protects {start:#010x}-{:#010x} \
                 as a boot loader section,\nand this image starts at {:#010x}: an erase there is \
                 not performed. Nothing was erased.\n\n\
                 --clear-bootprot clears it. It prints the part's user row, saves it, and changes \
                 only BOOTPROT.",
                protected_end - 1,
                image.base
            )));
        }
        self.refuse_locked_regions(image, walk_end, flash_bytes, &row)
    }

    /// Refuse an image whose erase walk reaches a locked region of a SAM D21's main array.
    ///
    /// The array is sixteen equal regions, and a region is locked while its bit in NVMCTRL.LOCK
    /// reads 0 (DS40001882D 22.6.3 and 22.8). The register is read as it stands, because a Lock or
    /// Unlock Region command changes it until the next reset, and `row` says whether the part
    /// sets each lock again at reset. The rows the EEPROM field reserves at the top of the array
    /// are written whatever their region's lock says (22.6.5 and Table 22-3), so the walk is
    /// checked only up to where they begin.
    fn refuse_locked_regions(
        &mut self,
        image: &Image<'_>,
        walk_end: u32,
        flash_bytes: u32,
        row: &Samd21UserRow,
    ) -> Result<(), FlashError> {
        let locks = self.target.read_samd21_region_locks()?;
        let start = self.family.flash_base();
        let region_bytes = flash_bytes / SAMD21_LOCK_REGIONS;
        let eeprom_start = start
            .saturating_add(flash_bytes)
            .saturating_sub(samd21_eeprom_bytes(row.eeprom()));
        let checked_end = walk_end.min(eeprom_start);
        if image.base >= checked_end {
            return Ok(());
        }
        let locked: Vec<(u32, u32, u32)> = (0..SAMD21_LOCK_REGIONS)
            .filter(|&region| u32::from(locks) & (1 << region) == 0)
            .map(|region| {
                let from = start.saturating_add(region.saturating_mul(region_bytes));
                (region, from, from.saturating_add(region_bytes))
            })
            .filter(|&(_, from, to)| from < checked_end && image.base < to)
            .collect();
        if locked.is_empty() {
            return Ok(());
        }
        let listed: String = locked
            .iter()
            .map(|(region, from, to)| {
                format!("\n    region {region:<2}  {from:#010x}-{:#010x}", to - 1)
            })
            .collect();
        let (what, regions, locks_were, commands, them) = if locked.len() == 1 {
            ("a locked region", "that region", "the lock was", "a Lock Region command", "it")
        } else {
            ("locked regions", "those regions", "the locks were", "Lock Region commands", "them")
        };
        let origin = if locked
            .iter()
            .all(|(region, _, _)| u32::from(row.lock()) & (1 << region) != 0)
        {
            format!(
                "The user row's LOCK field leaves {regions} unlocked at reset, so {locks_were} set \
                 since\nthe last reset, by {commands}. A reset lifts {them}, unless the program on \
                 the part locks\n{them} again as it starts."
            )
        } else {
            format!(
                "The user row's LOCK field is {:#06x}, and the part loads it at every reset \
                 (DS40001882D 22.6.3).\nNo option here changes that field.",
                row.lock()
            )
        };
        Err(FlashError::Refused(format!(
            "this part's NVMCTRL.LOCK reads {locks:#06x}, and this write erases rows in {what}:\n\
             an erase in a locked region is not performed. Nothing was erased.\n{listed}\n\n\
             {origin}"
        )))
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
        self.base
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
                     die would erase and program a part it cannot identify."
                )));
            };
            return Ok(PartIdentity { value: u64::from(cidr), what: part });
        }
        if let crate::SamIdentity::Sam4Chipid(families) = self.family.identity_register() {
            let chipid = |what: &str, why: &dyn core::fmt::Debug| {
                FlashError::Refused(format!(
                    "reading the SAM4 {what} at {SAM4_CHIPID_CIDR:#010x} failed: {why:?}.\n\n\
                     Two things do this and they call for opposite next steps. The part may not \
                     BE a SAM4 --\nthat address is a CHIPID on a SAM4 and unimplemented on a SAM \
                     D5x/E5x, and six Xplained Pro\nkits share one USB id, so a serial names a KIT \
                     SHAPE and never a part. Or the probe reached\nnothing at all, which reads \
                     identically here.\n\n\
                     `lamella devices` lists what is attached; naming a different --board for the \
                     same probe\nwill say which of the two it is."
                ))
            };
            let cidr =
                self.target.read_word(SAM4_CHIPID_CIDR).map_err(|why| chipid("CHIPID", &why))?;
            let exid =
                self.target.read_word(SAM4_CHIPID_EXID).map_err(|why| chipid("EXID", &why))?;
            if !families.iter().any(|family| sam4_family_matches(cidr, family)) {
                return Err(FlashError::Refused(format!(
                    "CHIPID reports CIDR {cidr:#010x} / EXID {exid:#010x}, which is not the {} \
                     this route drives -- refused rather than driven, because a flash routine \
                     pointed at the wrong controller would write flash registers that are not \
                     there on this part.",
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
        if self.kept.is_some() && !crate::placement::keeps_a_bootloader(self.family) {
            return Err(FlashError::Refused(format!(
                "this write keeps a bootloader in front of the image, and keeping one is built \
                 for a SAM D21, not a {}",
                self.family.controller()
            )));
        }
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
        let array_base = self.family.flash_base();
        let offset = image.base.wrapping_sub(array_base);
        if image.base < array_base || !offset.is_multiple_of(granule) {
            return Err(FlashError::Refused(format!(
                "this image starts at {:#010x}, which is not the start of one of this part's \
                 {granule}-byte erase units from {array_base:#010x}; erasing it would erase the \
                 bytes in front of it",
                image.base
            )));
        }
        let granules = wanted.div_ceil(granule);
        let walk_end = image.base.saturating_add(granules.saturating_mul(granule));
        let array_end = array_base.saturating_add(geometry.flash_bytes());
        if walk_end > array_end {
            return Err(FlashError::Refused(format!(
                "erasing {wanted} bytes from {:#010x} walks to {walk_end:#010x}, past the {} KB \
                 this part reports fitted, whose array ends at {array_end:#010x}",
                image.base,
                geometry.flash_bytes() / 1024
            )));
        }
        if let Some(kept) = self.kept.clone() {
            self.refuse_unless_the_bootloader_is_there(&kept)?;
        }
        if self.family == crate::SamFamily::Samd21 {
            self.refuse_protected_rows(image, walk_end, geometry.flash_bytes())?;
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
        leave_running(&mut self.target)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
