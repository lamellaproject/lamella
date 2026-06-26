//! Nordic nRF51 flash programming over a Lamella CMSIS-DAP debug probe.

use lamella_cmsis_dap::{Dap, DapError, Transport};

const NVMC_READY: u32 = 0x4001_e400;
const NVMC_CONFIG: u32 = 0x4001_e504;
const NVMC_ERASEPAGE: u32 = 0x4001_e508;
const NVMC_REN: u32 = 0;
const NVMC_WEN: u32 = 1;
const NVMC_EEN: u32 = 2;

/// nRF51 flash programming, added to a CMSIS-DAP [`Dap`] probe. Halt the core before erasing or
/// writing so it is not fetching from flash during the operation.
pub trait Nrf51Flash {
    /// Erases the flash page containing `address` (nRF51 pages are 1 KB) via the NVMC.
    fn erase_flash_page(&mut self, address: u32) -> Result<(), DapError>;
    /// Programs consecutive 32-bit `words` to flash starting at `address`, via the NVMC. The target
    /// pages must already be erased.
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), DapError>;
}

impl<T: Transport> Nrf51Flash for Dap<T> {
    fn erase_flash_page(&mut self, address: u32) -> Result<(), DapError> {
        self.write_word(NVMC_CONFIG, NVMC_EEN)?;
        nvmc_wait(self)?;
        self.write_word(NVMC_ERASEPAGE, address & !0x3ff)?;
        nvmc_wait(self)?;
        self.write_word(NVMC_CONFIG, NVMC_REN)
    }

    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), DapError> {
        self.write_word(NVMC_CONFIG, NVMC_WEN)?;
        nvmc_wait(self)?;
        for (i, &word) in words.iter().enumerate() {
            self.write_word(address + (i as u32) * 4, word)?;
            nvmc_wait(self)?;
        }
        self.write_word(NVMC_CONFIG, NVMC_REN)
    }
}

/// Polls the NVMC READY register until the controller is idle.
fn nvmc_wait<T: Transport>(dap: &mut Dap<T>) -> Result<(), DapError> {
    for _ in 0..1000 {
        if dap.read_word(NVMC_READY)? & 1 != 0 {
            return Ok(());
        }
    }
    Err(DapError::Timeout("flash controller"))
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
    Dap(DapError),
    /// Opening the probe failed (only from the `microbit` helper).
    ProbeOpen(String),
    /// A programmed word did not read back: flash verify failed at `word` (flash byte `word * 4`).
    Verify {
        word: usize,
        expected: u32,
        got: u32,
    },
}

impl From<DapError> for FlashError {
    fn from(e: DapError) -> Self {
        FlashError::Dap(e)
    }
}

/// Connect to the nRF51 over an open `dap`, erase the pages `image` spans, program it at `base`, verify
/// it word-for-word, and reset to run it -- the whole deploy dance (connect / halt / erase / write /
/// verify / reset) in one call instead of ~20 lines. The image is zero-padded up to a 32-bit word.
pub fn flash_and_run<T: Transport>(
    dap: &mut Dap<T>,
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

    dap.connect_swd()?;
    let idcode = dap.read_idcode()?;
    dap.init_mem()?;
    dap.halt()?;

    let pages = (words.len() * 4).div_ceil(0x400);
    for page in 0..pages as u32 {
        dap.erase_flash_page(base + page * 0x400)?;
    }
    dap.write_flash(base, &words)?;
    for (i, &expected) in words.iter().enumerate() {
        let got = dap.read_word(base + i as u32 * 4)?;
        if got != expected {
            return Err(FlashError::Verify {
                word: i,
                expected,
                got,
            });
        }
    }
    dap.reset_and_run()?;
    Ok(FlashReport {
        idcode,
        bytes: image.len(),
        words: words.len(),
    })
}

/// Open the BBC micro:bit's on-board CMSIS-DAP HID probe (VID 0x0d28, PID 0x0204) and
/// [`flash_and_run`] `image` at flash 0 -- the one-call deploy for a connected micro:bit.
#[cfg(feature = "microbit")]
pub fn flash_microbit(image: &[u8]) -> Result<FlashReport, FlashError> {
    let device = lamella_usbhid::Device::open(0x0d28, 0x0204, None)
        .map_err(|e| FlashError::ProbeOpen(format!("{e:?}")))?;
    let mut dap = Dap::new(device);
    flash_and_run(&mut dap, 0x0, image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cmsis_dap::proto;
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
        let mut dap = Dap::new(Mock::new(replies));
        dap.erase_flash_page(0x0003_f000).unwrap();
        assert_eq!(&dap.transport().sent[1][4..8], &2u32.to_le_bytes());
        assert_eq!(
            &dap.transport().sent[5][4..8],
            &0x0003_f000u32.to_le_bytes()
        );
    }

    #[test]
    fn write_flash_enables_then_writes_words() {
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
        let mut dap = Dap::new(Mock::new(replies));
        dap.write_flash(0x0003_f000, &[0xcafe_babe]).unwrap();
        assert_eq!(&dap.transport().sent[1][4..8], &1u32.to_le_bytes());
        assert_eq!(
            &dap.transport().sent[5][4..8],
            &0xcafe_babeu32.to_le_bytes()
        );
    }
}
