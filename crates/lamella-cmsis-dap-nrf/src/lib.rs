//! Nordic nRF51 flash programming over a Lamella debug probe.

use lamella_probe_core::{ProbeError, TargetAccess};

const NVMC_READY: u32 = 0x4001_e400;
const NVMC_CONFIG: u32 = 0x4001_e504;
const NVMC_ERASEPAGE: u32 = 0x4001_e508;
const NVMC_ERASEALL: u32 = 0x4001_e50c;
const NVMC_REN: u32 = 0;
const NVMC_WEN: u32 = 1;
const NVMC_EEN: u32 = 2;

/// The nRF51's flash page: 1 KB.
///
/// A PART FACT, NOT A CONVENIENT NUMBER. **The nRF52833 page is FOUR times this**, so a bare
/// `0x400` is wrong on every nRF52 while staying accidentally harmless: `flash_and_run` erases
/// every page before it writes any, so stepping 1 KB across a 4 KB part merely erases each page
/// four times -- correct, but ~512 erase operations where 128 would do, which at nRF52 erase
/// timings can make it SLOWER than the drag-and-drop it replaces. It stops being harmless the
/// moment an erase and a write interleave.
pub const NRF51_PAGE: u32 = 1024;

/// The nRF52833's flash page: 4 KB.
///
/// Recorded beside its sibling so the difference is visible at the point of use rather than
/// discovered. Nothing in this crate's page-erase path should use it: the nRF52 deploy goes
/// through [`erase_all`], which does no page arithmetic at all.
pub const NRF52833_PAGE: u32 = 4096;

/// Words per `write_flash` block, and per verify read-back: ONE 1 KB MEM-AP auto-increment window.
///
/// The window is the unit that costs something. A bulk transfer needs a fresh `TAR` at each 1 KB
/// boundary and at no other point (ADIv5 guarantees the MEM-AP's auto-increment only that far), so
/// a block that starts at a window boundary and ends at the next one pays exactly one `TAR` --
/// which is the architectural floor. Any other size pays more: a block SHORTER than a window pays
/// a `TAR` per block instead of per window, and a block that is not a whole number of windows
/// straddles a boundary and pays a second one on top.
///
/// This was `14 * 8` -- a whole number of 64-byte probe packets, on the reasoning that a stub
/// packet makes a batching win smaller than it looks. That reasoning weighs the wrong term. A stub
/// costs a fraction of a packet once per block; a misaligned window costs a whole extra round trip
/// on more than a third of them, plus the stub anyway on both sides of the split. Sizing to the
/// window instead spends one stub packet per window (256 is 18 packets and 4 words) to buy back
/// every one of those, and it cuts the READY polls in the write loop by the same ratio.
///
/// It assumes the caller's base address is 1 KB aligned, which a whole-image deploy is. A
/// misaligned base still programs correctly -- `write_words` splits at the boundary wherever it
/// falls -- it just pays the straddle this size exists to avoid.
const NVMC_WRITE_BLOCK: usize = 256;

/// The debug-port IDCODE an nRF51 (Cortex-M0) answers with -- a micro:bit v1.
pub const NRF51_IDCODE: u32 = 0x0bb1_1477;
/// The debug-port IDCODE an nRF52 (Cortex-M4) answers with -- a micro:bit v2.
pub const NRF52_IDCODE: u32 = 0x2ba0_1477;

/// nRF51 flash programming, added to a CMSIS-DAP [`TargetAccess`] probe. Halt the core before erasing or
/// writing so it is not fetching from flash during the operation.
pub trait Nrf51Flash {
    /// Erases the flash page containing `address` (nRF51 pages are 1 KB) via the NVMC.
    fn erase_flash_page(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Programs consecutive 32-bit `words` to flash starting at `address`, via the NVMC. The target
    /// pages must already be erased.
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError>;

    /// Erases the WHOLE main flash block in one NVMC operation, with no page arithmetic at all.
    ///
    /// The right primitive for programming a whole image, and the only one that is part-agnostic
    /// here: page erase needs a page size and this does not, so an nRF52833 is served correctly by
    /// the same call as an nRF51 without either learning the other's geometry. It is also far fewer
    /// operations than walking pages -- one, against 128 or 512.
    ///
    /// It erases everything, including anything the caller did not intend to replace. That is the
    /// right trade for a firmware deploy and the wrong one for a partial update.
    fn erase_all(&mut self) -> Result<(), ProbeError>;
}

impl<A: TargetAccess> Nrf51Flash for A {
    fn erase_flash_page(&mut self, address: u32) -> Result<(), ProbeError> {
        self.write_word(NVMC_CONFIG, NVMC_EEN)?;
        nvmc_wait(self)?;
        self.write_word(NVMC_ERASEPAGE, address & !(NRF51_PAGE - 1))?;
        nvmc_wait(self)?;
        self.write_word(NVMC_CONFIG, NVMC_REN)
    }

    fn erase_all(&mut self) -> Result<(), ProbeError> {
        self.write_word(NVMC_CONFIG, NVMC_EEN)?;
        nvmc_wait(self)?;
        self.write_word(NVMC_ERASEALL, 1)?;
        nvmc_wait(self)?;
        self.write_word(NVMC_CONFIG, NVMC_REN)
    }

    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        self.write_word(NVMC_CONFIG, NVMC_WEN)?;
        nvmc_wait(self)?;
        for (block, chunk) in words.chunks(NVMC_WRITE_BLOCK).enumerate() {
            let at = address + (block * NVMC_WRITE_BLOCK * 4) as u32;
            self.write_words(at, chunk)?;
            nvmc_wait(self)?;
        }
        self.write_word(NVMC_CONFIG, NVMC_REN)
    }
}

/// Reads the programmed image back and compares it word for word, in BLOCKS.
///
/// A per-word read costs TWO USB round trips per word -- an address and then the data register --
/// while `TargetAccess::read_words_into` writes the address once per 1 KB window and streams the
/// words under it, which on a firmware-sized image is the difference between minutes and seconds.
/// Unlike the write half there is no controller state to respect here at all: nothing is being
/// programmed, so nothing can be outrun.
///
/// The comparison stays word for word.
///
fn verify_flash<A: TargetAccess>(
    target: &mut A,
    base: u32,
    words: &[u32],
) -> Result<(), FlashError> {
    let mut buffer = alloc_block();
    for (block, expected) in words.chunks(NVMC_WRITE_BLOCK).enumerate() {
        let at = base + (block * NVMC_WRITE_BLOCK * 4) as u32;
        let got = &mut buffer[..expected.len()];
        target.read_words_into(at, got)?;
        for (i, (want, read)) in expected.iter().zip(got.iter()).enumerate() {
            if want != read {
                return Err(FlashError::Verify {
                    word: block * NVMC_WRITE_BLOCK + i,
                    expected: *want,
                    got: *read,
                });
            }
        }
    }
    Ok(())
}

/// One reusable read-back buffer, so a verify of a 448 KB image allocates once rather than per
/// block -- and so the same code shape works on a host that has an allocator and a master that
/// barely does.
fn alloc_block() -> Vec<u32> {
    vec![0; NVMC_WRITE_BLOCK]
}

/// Polls the NVMC READY register until the controller is idle.
fn nvmc_wait<A: TargetAccess>(target: &mut A) -> Result<(), ProbeError> {
    for _ in 0..1000 {
        if target.read_word(NVMC_READY)? & 1 != 0 {
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("flash controller"))
}

/// Outcome of a successful flash deploy.
#[derive(Debug, Clone, Copy)]
pub struct FlashReport {
    /// The target's DP IDCODE, read while connecting.
    pub idcode: u32,
    /// Bytes written to flash.
    pub bytes: usize,
    /// 32-bit words written (the image zero-padded up to a word).
    pub words: usize,
}

/// A reason a flash deploy failed.
#[derive(Debug)]
pub enum FlashError {
    /// A probe / debug-access error.
    Probe(ProbeError),
    /// Opening the probe failed (only from the `microbit` helper).
    ProbeOpen(String),
    /// A programmed word did not read back: flash verify failed at `word` (flash byte `word * 4`).
    Verify {
        /// Index of the first word that differed, counting from the start of the image.
        word: usize,
        /// The word the image says should be there.
        expected: u32,
        /// The word read back from flash.
        got: u32,
    },
    /// The debug port answered with a different part than the caller said it was deploying to --
    /// raised BEFORE anything is erased. A micro:bit v1 is `0x0bb11477` and a v2 is `0x2ba01477`,
    /// so this is the difference between "nothing happened" and "the wrong board was erased and
    /// then written with an image its core cannot execute".
    WrongPart {
        /// The IDCODE the caller's target part answers with.
        expected: u32,
        /// The IDCODE the debug port actually answered.
        found: u32,
    },
}

impl From<ProbeError> for FlashError {
    fn from(e: ProbeError) -> Self {
        FlashError::Probe(e)
    }
}

/// Connect to the nRF51 over an open `target`, erase the pages `image` spans, program it at `base`, verify
/// it word-for-word, and reset to run it -- the whole deploy dance (connect / halt / erase / write /
/// verify / reset) in one call instead of ~20 lines. The image is zero-padded up to a 32-bit word.
pub fn flash_and_run<A: TargetAccess>(
    target: &mut A,
    base: u32,
    image: &[u8],
) -> Result<FlashReport, FlashError> {
    let words: Vec<u32> = image
        .chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(w)
        })
        .collect();

    target.connect()?;
    let idcode = target.read_idcode()?;
    target.init_mem()?;
    target.halt()?;

    let pages = (words.len() * 4).div_ceil(NRF51_PAGE as usize);
    for page in 0..pages as u32 {
        target.erase_flash_page(base + page * NRF51_PAGE)?;
    }
    target.write_flash(base, &words)?;
    verify_flash(target, base, &words)?;
    target.reset_and_run()?;
    Ok(FlashReport {
        idcode,
        bytes: image.len(),
        words: words.len(),
    })
}

/// The BBC micro:bit's on-board CMSIS-DAP HID probe. **Every micro:bit ever made presents these
/// same two numbers**, which is why nothing here may open by them alone.
#[cfg(feature = "microbit")]
pub const MICROBIT_DAPLINK: (u16, u16) = (0x0d28, 0x0204);

/// Open a micro:bit's on-board probe and [`flash_and_run`] `image` at flash 0 -- the one-call
/// deploy. `serial` names WHICH micro:bit; `None` means the sole attached one.
///
/// **THIS WRITES FLASH, SO IT MUST NOT CHOOSE ITS TARGET BY USB ENUMERATION ORDER.** Opening
/// `0d28:0204` with no serial, so on a bench holding more than one micro:bit it reached whichever
/// the OS handed over first -- an order that changes with plug order and across reboots. **That
/// failure does not announce itself:** the flash succeeds, on someone else's board, and its owner
/// finds it running a program nobody sent it with nothing in any log to say why.
///
/// So the target is resolved through [`lamella_probe::resolve_serial`] -- an explicit `serial`,
/// then `LAMELLA_PROBE_SERIAL`, then the sole attached micro:bit, then a REFUSAL naming every
/// candidate. A one-board bench is unaffected; a multi-board bench can no longer be written to by
/// accident.
#[cfg(feature = "microbit")]
pub fn flash_microbit(image: &[u8], serial: Option<&str>) -> Result<FlashReport, FlashError> {
    let (vid, pid) = MICROBIT_DAPLINK;
    let chosen = lamella_probe::resolve_serial(vid, pid, serial)
        .map_err(|e| FlashError::ProbeOpen(e.to_string()))?;
    let device = lamella_usbhid::Device::open(vid, pid, Some(&chosen))
        .map_err(|e| FlashError::ProbeOpen(format!("{e:?}")))?;
    let mut target = lamella_probe_core::ArmDap::new(lamella_cmsis_dap::Dap::new(device));
    flash_and_run(&mut target, 0x0, image)
}

/// Erase the WHOLE flash, program `image` at `base`, verify it word for word, and reset to run it.
///
/// [`flash_and_run`]'s counterpart for a part whose page size this crate does not want to encode:
/// `ERASEALL` needs no geometry, so this one function is correct on an nRF51 and an nRF52833 alike.
///
/// IT REFUSES A PART IT DOES NOT EXPECT, and that guard is the reason to prefer it. `expect_idcode`
/// is checked against the debug port BEFORE anything is erased, so pointing a v2 image at a v1 board
/// stops at a message instead of erasing the board and then writing an image its core cannot run.
pub fn erase_all_and_run<A: TargetAccess>(
    target: &mut A,
    base: u32,
    image: &[u8],
    expect_idcode: u32,
) -> Result<FlashReport, FlashError> {
    let words: Vec<u32> = image
        .chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(w)
        })
        .collect();

    target.connect()?;
    let idcode = target.read_idcode()?;
    if idcode != expect_idcode {
        return Err(FlashError::WrongPart { expected: expect_idcode, found: idcode });
    }
    target.init_mem()?;
    target.halt()?;

    target.erase_all()?;
    target.write_flash(base, &words)?;
    verify_flash(target, base, &words)?;
    target.reset_and_run()?;
    Ok(FlashReport { idcode, bytes: image.len(), words: words.len() })
}

/// Open a micro:bit **v2**'s on-board probe and deploy `image` to flash 0 -- erase-all, program,
/// verify, reset -- refusing any board that is not an nRF52.
///
/// THIS EXISTS TO REPLACE THE MASS-STORAGE PATH, WHICH HAS NO VERIFY AND CANNOT BE TRUSTED TO
/// REPORT ITS OWN OUTCOME. Measured in one session: five failed flashes in three
/// different faces, including a hex file DAPLink called undecodable whose 28,744 records every
/// validated independently, a volume that remounted MID-COPY, and -- the one that should worry us --
/// **a `FAIL.TXT` whose CONTENT was "Operation was successful", which had vanished by the time the
/// volume was listed.** A harness that treats any `FAIL.TXT` as authoritative therefore reported a
/// SUCCESSFUL flash as a failure; and because the mass-storage path never reads the flash back,
/// nothing can rule out the reverse having happened. This path reads every word back.
///
/// `serial` names WHICH micro:bit, through the same [`lamella_probe::resolve_serial`] ladder
/// [`flash_microbit`] uses -- explicit, then `LAMELLA_PROBE_SERIAL`, then the sole attached board,
/// then a refusal naming every candidate. It writes flash, so it must never pick by enumeration
/// order.
#[cfg(feature = "microbit")]
pub fn flash_microbit_v2(image: &[u8], serial: Option<&str>) -> Result<FlashReport, FlashError> {
    let (vid, pid) = MICROBIT_DAPLINK;
    let chosen = lamella_probe::resolve_serial(vid, pid, serial)
        .map_err(|e| FlashError::ProbeOpen(e.to_string()))?;
    let device = lamella_usbhid::Device::open(vid, pid, Some(&chosen))
        .map_err(|e| FlashError::ProbeOpen(format!("{e:?}")))?;
    let mut target = lamella_probe_core::ArmDap::new(lamella_cmsis_dap::Dap::new(device));
    erase_all_and_run(&mut target, 0x0, image, NRF52_IDCODE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cmsis_dap::{Dap, proto};
    use lamella_probe_core::ArmDap;
    use lamella_cmsis_dap::testing::{Mock, echo};

    #[test]
    fn erase_flash_page_drives_nvmc() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let ready = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready,
            ack.clone(),
            ack,
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.erase_flash_page(0x0003_f000).unwrap();
        assert_eq!(&target.inner().transport().sent[1][4..8], &2u32.to_le_bytes());
        assert_eq!(
            &target.inner().transport().sent[5][4..8],
            &0x0003_f000u32.to_le_bytes()
        );
    }

    /// Pins the ORDER of the controller's states around the transfer rather than the shape of the
    /// transfer itself: `CONFIG` is set to `WEN` before any data moves, the payload leaves as a
    /// BLOCK transfer rather than a TAR/DRW pair per word, and `CONFIG` returns to `REN`.
    ///
    #[test]
    fn write_flash_enables_then_sends_a_block() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let ready = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00];
        let block_ok = vec![proto::cmd::TRANSFER_BLOCK, 0x04, 0x00, 0x01];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready.clone(),
            ack.clone(),
            block_ok,
            ack.clone(),
            ready,
            ack.clone(),
            ack,
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let payload = [0xcafe_babeu32, 0x1234_5678, 0xdead_beef, 0x0000_0001];
        target.write_flash(0x0003_f000, &payload).unwrap();

        let sent = target.inner().transport().sent.clone();
        assert_eq!(&sent[1][4..8], &NVMC_WEN.to_le_bytes(), "write must be enabled first");
        assert_eq!(sent.last().unwrap()[4..8], NVMC_REN.to_le_bytes(), "must return to read-only");

        let block_writes = sent.iter().filter(|p| p[0] == proto::cmd::TRANSFER_BLOCK).count();
        assert_eq!(block_writes, 1, "the payload must leave as ONE block transfer");
        assert!(
            sent.iter().any(|p| p[0] == proto::cmd::TRANSFER_BLOCK && p.len() >= 4 + payload.len() * 4),
            "the block must carry the whole payload"
        );
    }
}

#[cfg(test)]
mod erase_all_tests {
    use super::*;
    use lamella_cmsis_dap::testing::{Mock, echo};
    use lamella_cmsis_dap::{Dap, proto};
    use lamella_probe_core::ArmDap;

    fn ack() -> Vec<u8> {
        echo(proto::cmd::TRANSFER, &[0x01, 0x01])
    }
    fn ready() -> Vec<u8> {
        vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00]
    }

    /// ERASEALL drives the whole-block register and NEVER touches ERASEPAGE -- which is the point:
    /// no page arithmetic means no page SIZE, so this call is correct on an nRF51 and an nRF52833
    /// alike. A version that quietly walked pages would pass any test that only checked the flash
    /// contents afterwards.
    #[test]
    fn erase_all_drives_the_whole_block_and_never_a_page() {
        let replies = vec![
            ack(), ack(),
            ack(), ready(),
            ack(), ack(),
            ack(), ready(),
            ack(), ack(),
        ];
        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        target.erase_all().unwrap();

        let sent = target.inner().transport().sent.clone();
        assert_eq!(&sent[1][4..8], &NVMC_EEN.to_le_bytes(), "erase must be enabled first");
        assert_eq!(&sent[4][4..8], &NVMC_ERASEALL.to_le_bytes(), "must drive ERASEALL");
        assert_eq!(&sent[5][4..8], &1u32.to_le_bytes(), "ERASEALL is triggered by writing 1");
        assert!(
            !sent.iter().any(|p| p.len() >= 8 && p[4..8] == NVMC_ERASEPAGE.to_le_bytes()),
            "a whole-block erase must not touch the per-page register"
        );
        assert_eq!(sent.last().unwrap()[4..8], NVMC_REN.to_le_bytes(), "must return to read-only");
    }

    /// THE GUARD MUST FIRE BEFORE ANYTHING IS ERASED, and that ORDER is the property -- a guard
    /// that ran after the erase would have destroyed the thing it exists to protect while still
    /// returning the right error. So this asserts not merely that the wrong part is refused, but
    /// that the NVMC was never driven at all.
    #[test]
    fn a_wrong_part_is_refused_before_any_erase() {
        let idcode = {
            let b = NRF51_IDCODE.to_le_bytes();
            vec![proto::cmd::TRANSFER, 0x01, 0x01, b[0], b[1], b[2], b[3]]
        };
        let mut replies = vec![
            echo(proto::cmd::CONNECT, &[proto::Port::Swd as u8]),
            echo(proto::cmd::SWJ_CLOCK, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            idcode,
        ];
        replies.extend(std::iter::repeat_n(ack(), 40));

        let mut target = ArmDap::new(Dap::new(Mock::new(replies)));
        let outcome = erase_all_and_run(&mut target, 0x0, &[0xaa; 64], NRF52_IDCODE);

        match outcome {
            Err(FlashError::WrongPart { expected, found }) => {
                assert_eq!(expected, NRF52_IDCODE);
                assert_eq!(found, NRF51_IDCODE);
            }
            other => panic!("expected a WrongPart refusal, got {other:?}"),
        }
        let sent = target.inner().transport().sent.clone();
        assert!(
            !sent.iter().any(|p| p.len() >= 8 && p[4..8] == NVMC_CONFIG.to_le_bytes()),
            "the NVMC must never have been enabled -- the guard ran too late"
        );
    }
}
