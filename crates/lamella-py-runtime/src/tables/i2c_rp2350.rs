//! The Raspberry Pi RP2350 (Pico 2) I2C0 (a Synopsys DW_apb_i2c) register facts -- transcribed
//! from the neutral peripheral-table SSOT (sources: the official RP2350 SVD + the official
//! pico-sdk hardware_i2c). ic_clk is clk_sys, assumed 150 MHz by the
//! serve profile (a live native-USB wire proves the PLL is up), so the counts scale from there.

use crate::i2c::{I2cConfig, I2cConfigError, I2cFacts, I2cOp};
use alloc::vec;
use alloc::vec::Vec;

/// RESETS: the atomic-clear alias releases I2C0 (bit 4) + PADS_BANK0 (bit 9) + IO_BANK0 (bit 6);
/// the done register is reflected by the sim's reset-clear accumulator.
const RESETS_RESET_CLR: u32 = 0x4002_3000;
const RESETS_RESET_DONE: u32 = 0x4002_0008;
const RESET_I2C0_PADS_IO: u32 = (1 << 4) | (1 << 9) | (1 << 6);

/// IO_BANK0: GP4 = i2c0_sda, GP5 = i2c0_scl, both at FUNCSEL 3 (open-drain through the block).
const IO_BANK0_GPIO4_CTRL: u32 = 0x4002_8024;
const IO_BANK0_GPIO5_CTRL: u32 = 0x4002_802C;
const FUNCSEL_I2C: u32 = 3;

/// PADS_BANK0: de-isolate, input buffer, internal pull-up, keep the Schmitt trigger (0x4A).
const PADS_BANK0_GPIO4: u32 = 0x4003_8014;
const PADS_BANK0_GPIO5: u32 = 0x4003_8018;
const PAD_I2C: u32 = 0x4A;

/// The DW_apb_i2c block.
const I2C0: u32 = 0x4009_0000;
const IC_CON: u32 = I2C0;
const IC_TAR: u32 = I2C0 + 0x4;
const IC_DATA_CMD: u32 = I2C0 + 0x10;
const IC_FS_SCL_HCNT: u32 = I2C0 + 0x1C;
const IC_FS_SCL_LCNT: u32 = I2C0 + 0x20;
const IC_RAW_INTR_STAT: u32 = I2C0 + 0x34;
const IC_RX_TL: u32 = I2C0 + 0x38;
const IC_TX_TL: u32 = I2C0 + 0x3C;
const IC_CLR_TX_ABRT: u32 = I2C0 + 0x54;
const IC_CLR_STOP_DET: u32 = I2C0 + 0x60;
const IC_ENABLE: u32 = I2C0 + 0x6C;
const IC_STATUS: u32 = I2C0 + 0x70;
const IC_RXFLR: u32 = I2C0 + 0x78;
const IC_SDA_HOLD: u32 = I2C0 + 0x7C;
const IC_TX_ABRT_SOURCE: u32 = I2C0 + 0x80;
const IC_FS_SPKLEN: u32 = I2C0 + 0xA0;

/// IC_CON: master | fast-mode counts | restart-en | slave off | TX_EMPTY = completion.
const CON_MASTER_FAST_TXEMPTY: u32 = 0x165;

const STATUS_TFNF: u32 = 1 << 1;
const INTR_TX_EMPTY: u32 = 1 << 4;
const INTR_TX_ABRT: u32 = 1 << 6;
const INTR_STOP_DET: u32 = 1 << 9;
const CMD_READ: u32 = 1 << 8;
const CMD_STOP: u32 = 1 << 9;
const CMD_RESTART: u32 = 1 << 10;
const ABRT_7B_ADDR_NOACK: u32 = 1 << 0;
const ABRT_TXDATA_NOACK: u32 = 1 << 3;

/// ic_clk = clk_sys; the serve profile runs 150 MHz (a live native-USB wire proves the PLL).
const IC_CLK_HZ: u64 = 150_000_000;

pub(crate) fn facts() -> I2cFacts {
    I2cFacts {
        enable: IC_ENABLE,
        tar: IC_TAR,
        data_cmd: IC_DATA_CMD,
        raw_intr_stat: IC_RAW_INTR_STAT,
        abort_source: IC_TX_ABRT_SOURCE,
        clr_tx_abrt: IC_CLR_TX_ABRT,
        clr_stop_det: IC_CLR_STOP_DET,
        rxflr: IC_RXFLR,
        status: IC_STATUS,
        status_tfnf: STATUS_TFNF,
        intr_tx_empty: INTR_TX_EMPTY,
        intr_tx_abrt: INTR_TX_ABRT,
        intr_stop_det: INTR_STOP_DET,
        cmd_read: CMD_READ,
        cmd_stop: CMD_STOP,
        cmd_restart: CMD_RESTART,
        abrt_addr_nack: ABRT_7B_ADDR_NOACK,
        abrt_data_nack: ABRT_TXDATA_NOACK,
    }
}

/// The (HCNT, LCNT, SPKLEN, SDA_HOLD, realized-Hz) tuple for `rate`, by the official pico-sdk
/// i2c baud math off the resident 150 MHz clk_sys. The realized SCL rate uses the Synopsys period
/// (t_HIGH = HCNT + SPKLEN + 7, t_LOW = LCNT + 1) so the echo lands UNDER the request, as silicon
/// does (100 kHz -> ~96 kHz). `None` when a counter falls outside its expressible range.
fn counts(rate: u32) -> Option<(u32, u32, u32, u32, u32)> {
    let rate = u64::from(rate);
    if rate == 0 {
        return None;
    }
    let period = (IC_CLK_HZ + rate / 2) / rate;
    let lcnt = period * 3 / 5;
    let hcnt = period.checked_sub(lcnt)?;
    if !(8..=65535).contains(&hcnt) || !(8..=65535).contains(&lcnt) {
        return None;
    }
    let spklen = if lcnt < 16 { 1 } else { lcnt / 16 };
    let sda_hold = if rate < 1_000_000 {
        IC_CLK_HZ * 3 / 10_000_000 + 1
    } else {
        IC_CLK_HZ * 3 / 25_000_000 + 1
    };
    if sda_hold > lcnt.saturating_sub(2) {
        return None;
    }
    let realized = IC_CLK_HZ / (hcnt + lcnt + spklen + 8);
    Some((hcnt as u32, lcnt as u32, spklen as u32, sda_hold as u32, realized as u32))
}

/// The bring-up opening the block with `config`, paired with the realized SCL rate: un-reset
/// I2C0/PADS/IO, the SDA/SCL pads + pins, then IC_CON / watermarks / counts / hold / spike written
/// DISABLED and IC_ENABLE set last (the DW_apb_i2c latches those only while disabled).
pub(crate) fn open_ops(config: &I2cConfig) -> Result<(Vec<I2cOp>, u32), I2cConfigError> {
    let (hcnt, lcnt, spklen, sda_hold, realized) =
        counts(config.frequency).ok_or(I2cConfigError::FrequencyUnreachable)?;
    let ops = vec![
        I2cOp::Write { reg: RESETS_RESET_CLR, value: RESET_I2C0_PADS_IO },
        I2cOp::PollEq {
            reg: RESETS_RESET_DONE,
            mask: RESET_I2C0_PADS_IO,
            want: RESET_I2C0_PADS_IO,
        },
        I2cOp::Write { reg: PADS_BANK0_GPIO4, value: PAD_I2C },
        I2cOp::Write { reg: PADS_BANK0_GPIO5, value: PAD_I2C },
        I2cOp::Write { reg: IO_BANK0_GPIO4_CTRL, value: FUNCSEL_I2C },
        I2cOp::Write { reg: IO_BANK0_GPIO5_CTRL, value: FUNCSEL_I2C },
        I2cOp::Write { reg: IC_ENABLE, value: 0 },
        I2cOp::Write { reg: IC_CON, value: CON_MASTER_FAST_TXEMPTY },
        I2cOp::Write { reg: IC_RX_TL, value: 0 },
        I2cOp::Write { reg: IC_TX_TL, value: 0 },
        I2cOp::Write { reg: IC_FS_SCL_HCNT, value: hcnt },
        I2cOp::Write { reg: IC_FS_SCL_LCNT, value: lcnt },
        I2cOp::Write { reg: IC_FS_SPKLEN, value: spklen },
        I2cOp::Write { reg: IC_SDA_HOLD, value: sda_hold },
        I2cOp::Write { reg: IC_ENABLE, value: 1 },
    ];
    Ok((ops, realized))
}
