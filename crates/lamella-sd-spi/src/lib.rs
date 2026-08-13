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
    /// SWITCH_FUNC -- queries and selects the card's optional functions, of which the one that
    /// matters here is the bus speed. Command class 10; a card whose CSD omits that class does not
    /// have it. Answers R1 and then a 64-byte status as a data block, in BOTH modes.
    pub const SWITCH_FUNC: u8 = 6;
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

/// Encoding and decoding for [`cmd::SWITCH_FUNC`] -- the card's optional functions, six groups of
/// sixteen, of which group 1 is the bus speed.
///
/// **The CSD's `TRAN_SPEED` is the ceiling IN FORCE, not the ceiling AVAILABLE.** A card that
/// supports High Speed still reports the default-speed 25 MHz there until this command switches
/// it, at which point the same field reads 50 MHz. So a host that reads `TRAN_SPEED` and stops has
/// learned what the card is doing, not what it can do -- and the difference is a factor of two on
/// the wire.
pub mod switch {
    /// The 512-bit function status a card returns to [`cmd::SWITCH_FUNC`](super::cmd::SWITCH_FUNC),
    /// in either mode.
    pub const STATUS_LEN: usize = 64;

    /// Group 1 -- access mode, which is the bus speed. The other five groups are command system,
    /// driver strength, power limit and two reserved.
    pub const GROUP_ACCESS_MODE: u8 = 1;
    /// Group 1 function 1 -- High Speed (SDR25 in the later naming), whose ceiling is 50 MHz
    /// against default speed's 25.
    pub const FUNCTION_HIGH_SPEED: u8 = 1;
    /// The clock a card permits once [`FUNCTION_HIGH_SPEED`] is selected. Request it; a bus clamps
    /// to what its own divisor can produce.
    pub const HIGH_SPEED_CLOCK_HZ: u32 = 50_000_000;
    /// The nibble a card writes into a selection result when it did NOT switch -- "no influence",
    /// which is also what a host writes to leave a group alone.
    pub const NO_INFLUENCE: u8 = 0xF;

    /// The command argument. `set` chooses between checking (`false`, mode 0) and switching
    /// (`true`, mode 1); every group other than `group` is left at [`NO_INFLUENCE`].
    ///
    /// Mode 0 and mode 1 return the same status and differ only in whether the card ACTS on it. A
    /// card that does not offer a function answers mode 1 with [`NO_INFLUENCE`] in the result
    /// rather than with an error, so a switch whose result nobody reads looks exactly like one that
    /// worked.
    #[must_use]
    pub fn arg(set: bool, group: u8, function: u8) -> u32 {
        let mut groups = 0x00FF_FFFFu32;
        if (1..=6).contains(&group) {
            let shift = 4 * u32::from(group - 1);
            groups = (groups & !(0xF << shift)) | (u32::from(function & 0xF) << shift);
        }
        groups | (u32::from(set) << 31)
    }

    /// Whether `status` says the card OFFERS `function` in `group`.
    ///
    /// Group N's support bitmap is bits `[400 + 16(N-1) + 15 : 400 + 16(N-1)]`, and the status
    /// arrives most-significant bit first, so group 1 lands in bytes 12 and 13 and each later group
    /// two bytes earlier.
    #[must_use]
    pub fn supports(status: &[u8; STATUS_LEN], group: u8, function: u8) -> bool {
        if !(1..=6).contains(&group) || function > 15 {
            return false;
        }
        let high = 12 - 2 * usize::from(group - 1);
        let bitmap = (u16::from(status[high]) << 8) | u16::from(status[high + 1]);
        bitmap & (1 << function) != 0
    }

    /// The function `group` is CURRENTLY set to, as the card reports it -- [`NO_INFLUENCE`] when a
    /// switch was refused.
    ///
    /// The six result nibbles occupy bits 399:376, i.e. bytes 14 to 16, two groups per byte with
    /// the odd-numbered group in the low nibble.
    #[must_use]
    pub fn selected(status: &[u8; STATUS_LEN], group: u8) -> u8 {
        if !(1..=6).contains(&group) {
            return NO_INFLUENCE;
        }
        let index = group - 1;
        let byte = status[16 - usize::from(index / 2)];
        if index % 2 == 0 { byte & 0xF } else { byte >> 4 }
    }

    /// The maximum current the card draws, in milliamperes -- bits 511:496, the first two bytes.
    ///
    /// Worth reading before switching on a bus whose supply is marginal: High Speed raises it, and
    /// a card browned out mid-transfer reports as data corruption rather than as a power fault.
    #[must_use]
    pub fn max_current_ma(status: &[u8; STATUS_LEN]) -> u16 {
        (u16::from(status[0]) << 8) | u16::from(status[1])
    }
}

/// The sector count a CSD register describes.
///
/// `csd` is the 16 bytes as read from the card, and `is_mmc` says which family wrote them --
/// **which is not optional, because the same `CSD_STRUCTURE` value means different things on the
/// two families.** The field is the top two bits of byte 0:
///
/// | `CSD_STRUCTURE` | SD                          | MMC                          |
/// |-----------------|-----------------------------|------------------------------|
/// | `0b00`          | v1.0, exponent encoding     | v1.0, exponent encoding      |
/// | `0b01`          | v2.0, **512 KB unit count** | v1.1, **exponent encoding**  |
/// | `0b10`          | not defined                 | v1.2, exponent encoding      |
///
/// So `0b01` selects a completely different arithmetic depending on the family, and a decoder that
/// dispatched on the field alone would read an MMC card's capacity with the high-capacity SD
/// formula and return a number with no relation to the card. Every MMC layout uses the exponent
/// encoding; only SD ever uses the unit count.
///
/// A card whose structure is not in that table is REFUSED rather than guessed at.
pub fn csd_sector_count(csd: &[u8; 16], is_mmc: bool) -> BlockResult<u64> {
    if is_mmc {
        return match csd[0] >> 6 {
            0b00 | 0b01 | 0b10 => csd_exponent_sector_count(csd),
            _ => Err(BlockError::Io),
        };
    }
    match csd[0] >> 6 {
        0b00 => csd_exponent_sector_count(csd),
        0b01 => {
            let c_size =
                (u64::from(csd[7] & 0x3F) << 16) | (u64::from(csd[8]) << 8) | u64::from(csd[9]);
            Ok((c_size + 1) * 1024)
        }
        _ => Err(BlockError::Io),
    }
}

/// The exponent-and-multiplier encoding, shared by SD's v1 layout and by every MMC layout:
/// capacity = `(C_SIZE + 1) * 2^(C_SIZE_MULT + 2) * 2^READ_BL_LEN` bytes. Divided by the 512-byte
/// sector this seam presents, which is why the shift subtracts 9.
///
/// The fields sit at identical bit positions in both families, which is why one function serves
/// both -- but that is a fact worth stating rather than leaving to be noticed, because the
/// SELECTOR above them differs even where the arithmetic does not.
fn csd_exponent_sector_count(csd: &[u8; 16]) -> BlockResult<u64> {
    let c_size =
        (u64::from(csd[6] & 0x03) << 10) | (u64::from(csd[7]) << 2) | (u64::from(csd[8]) >> 6);
    let c_size_mult = (u64::from(csd[9] & 0x03) << 1) | (u64::from(csd[10]) >> 7);
    let read_bl_len = u64::from(csd[5] & 0x0F);
    if read_bl_len < 9 {
        return Err(BlockError::Io);
    }
    if c_size == 0xFFF {
        return Err(BlockError::Io);
    }
    Ok((c_size + 1) << (c_size_mult + 2 + read_bl_len - 9))
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
