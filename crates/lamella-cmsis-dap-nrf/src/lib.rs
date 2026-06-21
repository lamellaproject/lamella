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
