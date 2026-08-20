//! A memory card reached over SPI, presented as a [`BlockDevice`](lamella_cil_runtime::block).

#![cfg_attr(not(test), no_std)]

#[cfg(any(test, feature = "sim"))]
extern crate alloc;

pub mod card;

/// An in-memory card that answers the [`SdSpiBus`] seam, for host tests with no hardware. Behind
/// the `sim` feature (and always compiled under `cfg(test)`), so a device build never sees it.
#[cfg(any(test, feature = "sim"))]
pub mod sim;

use lamella_cil_runtime::block::{BlockError, BlockResult};

/// The SPI link a board supplies for a memory card.
///
/// Deliberately NOT the shape a per-transfer-chip-select bus takes. A card session holds
/// chip-select asserted ACROSS a command, its response and any data block, and initialization
/// requires clocking the bus with chip-select DEASSERTED -- so chip-select is the caller's to
/// drive, not a side effect of a transfer. The clock rate is on the trait for the same reason:
/// a card is identified slowly and then run fast, and only the board knows how to reprogram its
/// own divisor.
pub trait SdSpiBus {
    /// The bus's error type.
    type Error: core::fmt::Debug;

    /// Clocks `tx` out while capturing the simultaneously received bytes into `rx`. The two
    /// slices are equal length. Chip-select is NOT touched.
    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), Self::Error>;

    /// Drives chip-select: `true` asserts it (drives the line low on the wire).
    fn set_chip_select(&mut self, asserted: bool);

    /// Reprograms the bus clock. Called with a rate inside [`INIT_CLOCK_HZ_MAX`] during
    /// identification and with the board's working rate afterwards.
    fn set_clock_hz(&mut self, hz: u32);

    /// Sleeps at least `ms` milliseconds -- the card's power-up settle time.
    fn delay_ms(&mut self, ms: u32);
}

pub use lamella_sd_core::{
    ACMD41_HCS, BUS_WIDTH_4BIT, CsdError, INIT_CLOCKS_MIN, INIT_CLOCK_HZ_MAX, OCR_BUSY_COMPLETE,
    OCR_CCS, POWER_UP_SETTLE_MS, SEND_IF_COND_ARG, SEND_IF_COND_CHECK_PATTERN, cmd, switch,
};

/// Status bits of the one-byte R1 response. `0x00` means accepted with nothing to report; the
/// top bit is always clear, which is how a driver finds the response byte in a stream of `0xFF`
/// idle bytes.
pub mod r1 {
    /// The card is still initializing. Cleared once it is ready.
    pub const IDLE_STATE: u8 = 1 << 0;
    /// An erase sequence was cancelled.
    pub const ERASE_RESET: u8 = 1 << 1;
    /// The card did not recognize the command -- how a v1 card refuses `SEND_IF_COND`.
    pub const ILLEGAL_COMMAND: u8 = 1 << 2;
    /// The CRC of the last command did not check.
    pub const COM_CRC_ERROR: u8 = 1 << 3;
    /// An erase sequence was out of order.
    pub const ERASE_SEQUENCE_ERROR: u8 = 1 << 4;
    /// A misaligned address.
    pub const ADDRESS_ERROR: u8 = 1 << 5;
    /// An argument outside the card's range.
    pub const PARAMETER_ERROR: u8 = 1 << 6;
}

/// Control tokens that frame data blocks.
pub mod token {
    /// Precedes a data block for a single-block read, a single-block write, and every block of a
    /// multiple-block READ.
    pub const START_BLOCK: u8 = 0xFE;
    /// Precedes each block of a multiple-block WRITE -- a different token from
    /// [`START_BLOCK`] precisely so the card can tell the two streams apart.
    pub const START_BLOCK_MULTI_WRITE: u8 = 0xFC;
    /// Ends a multiple-block write, in the position a block token would occupy.
    pub const STOP_TRAN: u8 = 0xFD;
}

/// The one-byte token a card returns after each written data block.
///
/// Its layout is `x x x 0 s s s 1`: the three status bits sit in bits 3:1 with a fixed `0` above
/// and a fixed `1` below, and the top three bits are undefined. That is why the useful test masks
/// the low FIVE bits rather than comparing the whole byte -- the mask is the encoding, not a
/// superstition.
pub mod data_response {
    /// The bits of the response that carry meaning.
    pub const MASK: u8 = 0x1F;
    /// Status `010`: the card took the block.
    pub const ACCEPTED: u8 = 0x05;
    /// Status `101`: rejected, the block's CRC did not check.
    pub const REJECTED_CRC_ERROR: u8 = 0x0B;
    /// Status `110`: rejected, the card failed to write it.
    pub const REJECTED_WRITE_ERROR: u8 = 0x0D;

    /// Classifies a data-response byte, or `None` if it is not one.
    #[must_use]
    pub fn classify(byte: u8) -> Option<Result<(), super::WriteRejection>> {
        match byte & MASK {
            ACCEPTED => Some(Ok(())),
            REJECTED_CRC_ERROR => Some(Err(super::WriteRejection::CrcError)),
            REJECTED_WRITE_ERROR => Some(Err(super::WriteRejection::WriteError)),
            _ => None,
        }
    }
}

/// Why a card refused a written block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteRejection {
    /// The block's CRC did not check on arrival.
    CrcError,
    /// The card accepted the block but could not commit it.
    WriteError,
}

impl WriteRejection {
    /// Both rejections are medium failures to the layer above.
    #[must_use]
    pub fn to_block_error(self) -> BlockError {
        BlockError::Io
    }
}

/// Bytes in a command frame. Named so a caller batching a frame into one bus transfer sizes its
/// buffer from the frame rather than from a literal that could drift away from it.
pub const COMMAND_FRAME_LEN: usize = 6;

/// Builds the 6-byte command frame for `index` and `arg`, CRC included.
///
/// The CRC is computed rather than tabulated for the two commands that need one, because a driver
/// that hardcodes only those two cannot later turn CRC checking on.
#[must_use]
pub fn command_frame(index: u8, arg: u32) -> [u8; 6] {
    let mut frame = [
        0x40 | (index & 0x3F),
        (arg >> 24) as u8,
        (arg >> 16) as u8,
        (arg >> 8) as u8,
        arg as u8,
        0,
    ];
    frame[5] = (crc7(&frame[..5]) << 1) | 1;
    frame
}

/// CRC7 over `bytes`, returned in the low 7 bits. The generator is `x^7 + x^3 + 1`.
#[must_use]
pub fn crc7(bytes: &[u8]) -> u8 {
    const GENERATOR: u8 = 0x89;
    let mut crc: u8 = 0;
    for &byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ (GENERATOR << 1) } else { crc << 1 };
        }
    }
    crc >> 1
}

/// The sector count a CSD register describes, as this crate's [`BlockResult`].
///
/// A thin adapter over [`lamella_sd_core::csd_sector_count`], which holds the decode and the
/// SD-versus-MMC fork. The core crate deliberately names no block seam, so the mapping to
/// `BlockError` happens here rather than there.
///
/// **The core function distinguishes THREE refusals and this signature flattens them into one.**
/// An unknown structure, a sub-sector block length and an oversized-MMC sentinel are three
/// different things to go and look at. Call the core function directly when the reason matters.
pub fn csd_sector_count(csd: &[u8; 16], is_mmc: bool) -> BlockResult<u64> {
    lamella_sd_core::csd_sector_count(csd, is_mmc).map_err(|_| BlockError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc7_reproduces_the_two_published_constant_frames() {
        assert_eq!(command_frame(cmd::GO_IDLE_STATE, 0), [0x40, 0x00, 0x00, 0x00, 0x00, 0x95]);
        assert_eq!(
            command_frame(cmd::SEND_IF_COND, SEND_IF_COND_ARG),
            [0x48, 0x00, 0x00, 0x01, 0xAA, 0x87]
        );
    }

    #[test]
    fn a_command_frame_carries_its_index_and_argument() {
        let frame = command_frame(cmd::READ_SINGLE_BLOCK, 0xDEAD_BEEF);
        assert_eq!(frame[0], 0x40 | 17);
        assert_eq!(&frame[1..5], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(frame[5] & 1, 1);
    }

    #[test]
    fn the_data_response_mask_ignores_the_undefined_high_bits() {
        assert_eq!(data_response::classify(0b0000_0101), Some(Ok(())));
        assert_eq!(data_response::classify(0b1110_0101), Some(Ok(())));
        assert_eq!(
            data_response::classify(0b1110_1011),
            Some(Err(WriteRejection::CrcError))
        );
        assert_eq!(
            data_response::classify(0b1110_1101),
            Some(Err(WriteRejection::WriteError))
        );
        assert_eq!(data_response::classify(0xFF), None);
    }

    #[test]
    fn csd_v2_counts_512kb_units() {
        let mut csd = [0u8; 16];
        csd[0] = 0x40;
        let c_size: u32 = 8191;
        csd[7] = ((c_size >> 16) & 0x3F) as u8;
        csd[8] = ((c_size >> 8) & 0xFF) as u8;
        csd[9] = (c_size & 0xFF) as u8;
        assert_eq!(csd_sector_count(&csd, false), Ok(8192 * 1024));
        assert_eq!(8192u64 * 1024 * 512, 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn csd_v1_uses_the_exponent_encoding() {
        let mut csd = [0u8; 16];
        csd[0] = 0x00;
        csd[5] = 9;
        let c_size: u32 = 3751;
        csd[6] = ((c_size >> 10) & 0x03) as u8;
        csd[7] = ((c_size >> 2) & 0xFF) as u8;
        csd[8] = ((c_size & 0x03) << 6) as u8;
        let c_size_mult: u32 = 5;
        csd[9] = ((c_size_mult >> 1) & 0x03) as u8;
        csd[10] = ((c_size_mult & 1) << 7) as u8;
        assert_eq!(csd_sector_count(&csd, false), Ok(3752 * 128));
    }

    #[test]
    fn a_csd_claiming_a_sub_sector_block_is_rejected_not_wrapped() {
        let mut csd = [0u8; 16];
        csd[0] = 0x00;
        csd[5] = 4;
        assert_eq!(csd_sector_count(&csd, false), Err(BlockError::Io));
    }

    #[test]
    fn an_unknown_csd_version_is_refused() {
        let mut csd = [0u8; 16];
        csd[0] = 0xC0;
        assert_eq!(csd_sector_count(&csd, false), Err(BlockError::Io));
    }

    const MMC_128MB_CSD: [u8; 16] = [
        0x90, 0x26, 0x01, 0x2a, 0x0f, 0x59, 0x00, 0xf4, 0xf6, 0xdb, 0x1f, 0xff, 0x92, 0x40, 0x40,
        0x2f,
    ];

    #[test]
    fn an_mmc_csd_structure_2_decodes_with_the_exponent_encoding() {
        assert_eq!(csd_sector_count(&MMC_128MB_CSD, true), Ok(250_880));
        assert_eq!(250_880u64 * 512, 128_450_560);
    }

    #[test]
    fn the_same_register_read_as_sd_is_refused_rather_than_misdecoded() {
        assert_eq!(csd_sector_count(&MMC_128MB_CSD, false), Err(BlockError::Io));
    }

    #[test]
    fn structure_1_means_different_arithmetic_on_the_two_families() {
        let mut csd = [0u8; 16];
        csd[0] = 0x40;
        csd[5] = 9;
        csd[7] = 0xF4;
        csd[8] = 0xF6;
        csd[9] = 0xDB;
        csd[10] = 0x1F;
        let as_sd = csd_sector_count(&csd, false).unwrap();
        let as_mmc = csd_sector_count(&csd, true).unwrap();
        assert_ne!(
            as_sd, as_mmc,
            "if these agree the family argument is doing nothing and the bug is back"
        );
        assert_eq!(as_sd, 3_554_373_632);
        assert_eq!(as_mmc, 250_880);
    }

    #[test]
    fn a_high_capacity_mmc_is_refused_by_its_sentinel_not_given_a_wrong_size() {
        let mut csd = [0u8; 16];
        csd[0] = 0x80;
        csd[5] = 9;
        csd[6] = 0x03;
        csd[7] = 0xFF;
        csd[8] = 0xC0;
        assert_eq!(csd_sector_count(&csd, true), Err(BlockError::Io));
    }
}
