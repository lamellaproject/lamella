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

/// The upper bound on the clock rate during card identification.
///
/// The published simplified specification defers the exact figure to a bus-timing section it
/// leaves blank, so this rests on the widely-agreed 100-400 kHz identification band. Treated as a
/// ceiling, not a target.
pub const INIT_CLOCK_HZ_MAX: u32 = 400_000;

/// Clock cycles the host must supply, with chip-select and the data-out line high, before the
/// first command -- after the supply has been stable for at least
/// [`POWER_UP_SETTLE_MS`]. The card may spend them preparing itself.
///
/// Sent as whole bytes, so a driver rounds up: 10 bytes of `0xFF` is 80 cycles.
pub const INIT_CLOCKS_MIN: u32 = 74;

/// Milliseconds the supply must be stable before those clocks are supplied.
pub const POWER_UP_SETTLE_MS: u32 = 1;

/// The SPI-mode command set this driver uses. A command frame is
/// `[0x40 | index, arg[31:24], arg[23:16], arg[15:8], arg[7:0], crc7 << 1 | 1]`.
pub mod cmd {
    /// GO_IDLE_STATE -- software reset; the command that puts a card into SPI mode.
    pub const GO_IDLE_STATE: u8 = 0;
    /// SEND_IF_COND -- voltage/version probe. Present on v2 cards, illegal on v1.
    pub const SEND_IF_COND: u8 = 8;
    /// SEND_CSD -- the card-specific data register, which carries the capacity.
    pub const SEND_CSD: u8 = 9;
    /// SET_BLOCKLEN -- fixes the transfer block length (standard-capacity cards only).
    pub const SET_BLOCKLEN: u8 = 16;
    /// READ_SINGLE_BLOCK.
    pub const READ_SINGLE_BLOCK: u8 = 17;
    /// READ_MULTIPLE_BLOCK -- terminated by [`STOP_TRANSMISSION`].
    pub const READ_MULTIPLE_BLOCK: u8 = 18;
    /// WRITE_BLOCK.
    pub const WRITE_BLOCK: u8 = 24;
    /// WRITE_MULTIPLE_BLOCK -- terminated by the stop-tran TOKEN, not by a command.
    pub const WRITE_MULTIPLE_BLOCK: u8 = 25;
    /// STOP_TRANSMISSION -- ends a multiple-block READ.
    pub const STOP_TRANSMISSION: u8 = 12;
    /// APP_CMD -- the prefix that makes the NEXT command application-specific.
    pub const APP_CMD: u8 = 55;
    /// READ_OCR -- carries the card-capacity status bit after initialization.
    pub const READ_OCR: u8 = 58;
    /// SD_SEND_OP_COND -- an APPLICATION command: send [`APP_CMD`] first. Polled until the card
    /// leaves the idle state.
    pub const APP_SEND_OP_COND: u8 = 41;
    /// SEND_OP_COND -- the MultiMediaCard's NATIVE initialization command (CMD1), the counterpart
    /// to SD's [`APP_SEND_OP_COND`]. An SD card refuses it; an MMC is driven ready by polling it.
    /// It is how the init ladder tells MMC from SD once [`SEND_IF_COND`] has ruled out SD v2.
    pub const SEND_OP_COND: u8 = 1;
}

/// The argument [`cmd::SEND_IF_COND`] carries: the 2.7-3.6 V supply code in bits 11:8 and an
/// arbitrary check pattern in bits 7:0 that a card echoes back verbatim.
///
/// The echo is what makes this a real probe rather than a formality -- a card that answers
/// without returning the pattern has not understood the command.
pub const SEND_IF_COND_ARG: u32 = 0x0000_01AA;

/// The check-pattern byte of [`SEND_IF_COND_ARG`], for verifying the echo.
pub const SEND_IF_COND_CHECK_PATTERN: u8 = 0xAA;

/// The high-capacity-support bit in the [`cmd::APP_SEND_OP_COND`] argument.
///
/// The card samples it on the FIRST such command only, so a driver must send the same argument
/// every time it polls.
pub const ACMD41_HCS: u32 = 1 << 30;

/// The card-capacity-status bit in the OCR returned by [`cmd::READ_OCR`] (byte 0, bit 6 of the
/// 32-bit register read big-endian). Set means a block-addressed card.
pub const OCR_CCS: u32 = 1 << 30;

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

/// The sector count a CSD register describes, for both layouts.
///
/// `csd` is the 16 bytes as read from the card. The version lives in the top two bits of byte 0:
/// `0b00` is the v1 layout (an exponent-and-multiplier encoding) and `0b01` is the v2 layout (a
/// plain count of 512 KB units). Anything else is a layout this driver does not know.
pub fn csd_sector_count(csd: &[u8; 16]) -> BlockResult<u64> {
    match csd[0] >> 6 {
        0b00 => {
            let c_size =
                (u64::from(csd[6] & 0x03) << 10) | (u64::from(csd[7]) << 2) | (u64::from(csd[8]) >> 6);
            let c_size_mult = (u64::from(csd[9] & 0x03) << 1) | (u64::from(csd[10]) >> 7);
            let read_bl_len = u64::from(csd[5] & 0x0F);
            if read_bl_len < 9 {
                return Err(BlockError::Io);
            }
            Ok((c_size + 1) << (c_size_mult + 2 + read_bl_len - 9))
        }
        0b01 => {
            let c_size =
                (u64::from(csd[7] & 0x3F) << 16) | (u64::from(csd[8]) << 8) | u64::from(csd[9]);
            Ok((c_size + 1) * 1024)
        }
        _ => Err(BlockError::Io),
    }
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
        assert_eq!(csd_sector_count(&csd), Ok(8192 * 1024));
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
        assert_eq!(csd_sector_count(&csd), Ok(3752 * 128));
    }

    #[test]
    fn a_csd_claiming_a_sub_sector_block_is_rejected_not_wrapped() {
        let mut csd = [0u8; 16];
        csd[0] = 0x00;
        csd[5] = 4;
        assert_eq!(csd_sector_count(&csd), Err(BlockError::Io));
    }

    #[test]
    fn an_unknown_csd_version_is_refused() {
        let mut csd = [0u8; 16];
        csd[0] = 0xC0;
        assert_eq!(csd_sector_count(&csd), Err(BlockError::Io));
    }
}
