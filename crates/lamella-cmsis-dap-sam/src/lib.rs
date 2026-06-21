//! Microchip SAM (Atmel SAM D21 and family) flash programming over a Lamella CMSIS-DAP debug probe.

use lamella_cmsis_dap::{Dap, DapError, Transport};

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

/// SAM D21 (ATSAMD21) flash programming, added to a CMSIS-DAP [`Dap`] probe. Halt the core before
/// erasing or writing so it is not fetching from flash during the operation.
pub trait Samd21Flash {
    /// Erases the flash row (256 bytes) containing `address`, via the NVMCTRL.
    fn erase_flash_row(&mut self, address: u32) -> Result<(), DapError>;
    /// Programs consecutive 32-bit `words` to flash from `address`, via the NVMCTRL, one 64-byte
    /// page at a time (the rows must already be erased).
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), DapError>;
}

impl<T: Transport> Samd21Flash for Dap<T> {
    fn erase_flash_row(&mut self, address: u32) -> Result<(), DapError> {
        self.write_word(SAMD21_ADDR, (address & !(SAMD21_ROW - 1)) / 2)?;
        samd21_command(self, SAMD21_CMD_ER)
    }

    /// Manual write, per datasheet 22.6.4.3.1: clear the page buffer, fill it through the flash
    /// address space, issue a read-memory barrier, set the page address, then Write-Page.
    fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), DapError> {
        let ctrlb = self.read_word(SAMD21_CTRLB)?;
        self.write_word(SAMD21_CTRLB, ctrlb | SAMD21_MANW)?;
        for (page, chunk) in words.chunks(SAMD21_PAGE / 4).enumerate() {
            let page_addr = address + (page as u32) * SAMD21_PAGE as u32;
            samd21_command(self, SAMD21_CMD_PBC)?;
            for (i, &word) in chunk.iter().enumerate() {
                self.write_word(page_addr + (i as u32) * 4, word)?;
            }
            self.read_word(page_addr)?;
            self.write_word(SAMD21_ADDR, page_addr / 2)?;
            samd21_command(self, SAMD21_CMD_WP)?;
        }
        Ok(())
    }
}

/// Issues an NVMCTRL command (CMDEX key + `cmd`) and waits for the controller to be ready.
fn samd21_command<T: Transport>(dap: &mut Dap<T>, cmd: u32) -> Result<(), DapError> {
    dap.write_word(SAMD21_CTRLA, SAMD21_CMDEX | cmd)?;
    for _ in 0..1000 {
        if dap.read_word(SAMD21_INTFLAG)? & 1 != 0 {
            return Ok(());
        }
    }
    Err(DapError::Timeout("SAMD21 flash controller"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cmsis_dap::proto;
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
        let mut dap = Dap::new(Mock::new(replies));
        dap.erase_flash_row(0x0000_0100).unwrap();
        assert_eq!(&dap.transport().sent[1][4..8], &0x80u32.to_le_bytes());
        assert_eq!(
            &dap.transport().sent[3][4..8],
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
            ack.clone(),
            ack.clone(),
            flash,
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready,
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.write_flash(0x0, &[0xcafe_babe]).unwrap();
        assert_eq!(&dap.transport().sent[3][4..8], &0x80u32.to_le_bytes());
        assert_eq!(
            &dap.transport().sent[9][4..8],
            &0xcafe_babeu32.to_le_bytes()
        );
        assert_eq!(
            &dap.transport().sent[15][4..8],
            &0x0000_a504u32.to_le_bytes()
        );
    }
}
