//! The half of the SD card protocol that does not depend on which wire carries it.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

/// The command set, by index.
///
/// An index is the same number on both wires -- a command frame in SPI mode spells it
/// `0x40 | index`, and a native controller writes it into a command register -- so the indices
/// live here and each transport does its own framing.
pub mod cmd {
    /// GO_IDLE_STATE -- software reset. In SPI mode it is also the command that SELECTS SPI mode.
    pub const GO_IDLE_STATE: u8 = 0;
    /// SEND_OP_COND -- the MultiMediaCard's NATIVE initialization command (CMD1), the counterpart
    /// to SD's [`APP_SEND_OP_COND`]. An SD card refuses it; an MMC is driven ready by polling it.
    pub const SEND_OP_COND: u8 = 1;
    /// ALL_SEND_CID -- native mode only. Every card on the bus answers with its CID, which is how
    /// identification begins when the host cannot address a card it has not yet numbered.
    ///
    /// **There is no SPI-mode counterpart**: SPI has one card, selected by chip-select, so the
    /// whole identification-and-addressing phase collapses away.
    pub const ALL_SEND_CID: u8 = 2;
    /// SEND_RELATIVE_ADDR -- native mode only. The card proposes its own address (RCA), which the
    /// host then uses to talk to it specifically. Answered with R6.
    pub const SEND_RELATIVE_ADDR: u8 = 3;
    /// SWITCH_FUNC -- queries and selects the card's optional functions, of which the one that
    /// matters here is the bus speed. Command class 10; a card whose CSD omits that class does not
    /// have it. Answers with a 64-byte status as a data block, in BOTH modes.
    ///
    /// **The High Speed function is reachable in native mode and effectively is not over SPI**,
    /// where the CSD's `TRAN_SPEED` is the working ceiling instead. See [`switch`] for the
    /// encoding.
    pub const SWITCH_FUNC: u8 = 6;
    /// SELECT/DESELECT_CARD -- native mode only. Moves the addressed card into the transfer state;
    /// addressing zero deselects. Nothing but a selected card answers a data command.
    pub const SELECT_CARD: u8 = 7;
    /// SEND_IF_COND -- voltage/version probe. Present on v2 cards, illegal on v1.
    pub const SEND_IF_COND: u8 = 8;
    /// SEND_CSD -- the card-specific data register, which carries the capacity.
    pub const SEND_CSD: u8 = 9;
    /// SEND_STATUS -- the addressed card's status register, which is how a native-mode host waits
    /// out programming rather than watching a busy line.
    pub const SEND_STATUS: u8 = 13;
    /// STOP_TRANSMISSION -- ends a multiple-block READ.
    pub const STOP_TRANSMISSION: u8 = 12;
    /// SET_BLOCKLEN -- fixes the transfer block length (standard-capacity cards only).
    pub const SET_BLOCKLEN: u8 = 16;
    /// READ_SINGLE_BLOCK.
    pub const READ_SINGLE_BLOCK: u8 = 17;
    /// READ_MULTIPLE_BLOCK -- terminated by [`STOP_TRANSMISSION`].
    pub const READ_MULTIPLE_BLOCK: u8 = 18;
    /// WRITE_BLOCK.
    pub const WRITE_BLOCK: u8 = 24;
    /// WRITE_MULTIPLE_BLOCK -- in SPI mode terminated by the stop-tran TOKEN rather than a command.
    pub const WRITE_MULTIPLE_BLOCK: u8 = 25;
    /// SET_BUS_WIDTH -- an APPLICATION command (ACMD6), native mode only: selects the 1-bit or
    /// 4-bit data bus. **This is the command the whole four-wire path exists for.**
    pub const APP_SET_BUS_WIDTH: u8 = 6;
    /// SD_SEND_OP_COND -- an APPLICATION command (ACMD41): send [`APP_CMD`] first. Polled until
    /// the card leaves the idle state.
    pub const APP_SEND_OP_COND: u8 = 41;
    /// APP_CMD -- the prefix that makes the NEXT command application-specific.
    pub const APP_CMD: u8 = 55;
    /// READ_OCR -- carries the card-capacity status bit after initialization.
    ///
    /// SPI mode only. A native-mode host reads the OCR out of the R3 response to
    /// [`APP_SEND_OP_COND`] instead, so there is nothing to send.
    pub const READ_OCR: u8 = 58;
}

/// The argument [`cmd::SEND_IF_COND`] carries: the 2.7-3.6 V supply code in bits 11:8 and an
/// arbitrary check pattern in bits 7:0 that a card echoes back verbatim.
///
/// The echo is what makes this a real probe rather than a formality -- a card that answers
/// without returning the pattern has not understood the command.
pub const SEND_IF_COND_ARG: u32 = 0x0000_01AA;

/// The check-pattern byte of [`SEND_IF_COND_ARG`], for verifying the echo.
pub const SEND_IF_COND_CHECK_PATTERN: u8 = 0xAA;

/// The supported-voltage window a host advertises in the [`cmd::APP_SEND_OP_COND`] argument:
/// OCR bits 23:15, which are the 2.7 V to 3.6 V range every card in this class runs at.
///
/// **A zero window makes the command an INQUIRY rather than an initialization.** The card answers
/// with its own OCR and does NOT begin initializing, so a host that omits this polls a perfectly
/// healthy card forever and concludes it never became ready.
///
/// It is NOT needed on the SPI transport: there the argument carries only [`ACMD41_HCS`] and the
/// voltage negotiation does not happen. The same command, on two wires, wants two different arguments.
pub const ACMD41_VOLTAGE_WINDOW: u32 = 0x00FF_8000;

/// The high-capacity-support bit in the [`cmd::APP_SEND_OP_COND`] argument.
///
/// The card samples it on the FIRST such command only, so a driver must send the same argument
/// every time it polls.
pub const ACMD41_HCS: u32 = 1 << 30;

/// The card-capacity-status bit in the OCR. Set means a block-addressed (high-capacity) card.
///
/// Reached two ways: SPI hosts send [`cmd::READ_OCR`], native hosts read it out of the R3 response
/// to [`cmd::APP_SEND_OP_COND`]. Same bit, same meaning.
pub const OCR_CCS: u32 = 1 << 30;

/// The busy bit in the OCR, INVERTED as the card reports it: set means initialization is COMPLETE.
///
/// It shares a register with [`OCR_CCS`] and is the reason the initialization poll terminates, so
/// reading the capacity bit before this one is set reads a field the card has not finished writing.
pub const OCR_BUSY_COMPLETE: u32 = 1 << 31;

/// The 4-bit selection for [`cmd::APP_SET_BUS_WIDTH`]. `0` selects the 1-bit bus.
pub const BUS_WIDTH_4BIT: u32 = 2;

/// The clock a card permits once identification is over and before any function switch: the
/// default-speed ceiling of 25 MHz that essentially every card reports in the CSD's `TRAN_SPEED`.
///
/// **Identification happens at [`INIT_CLOCK_HZ_MAX`] and NOTHING raises the clock by itself.** A
/// ladder that finishes identifying and starts transferring is still running at 400 kHz, which is
/// a sixty-fold handicap that looks like a slow card rather than an unfinished driver -- it
/// transfers correctly, so nothing fails.
///
/// A host may raise this further only by asking the card, which is what [`switch`] is for.
pub const DEFAULT_SPEED_CLOCK_HZ: u32 = 25_000_000;

/// The upper bound on the clock rate during card identification.
///
/// The published simplified specification defers the exact figure to a bus-timing section it
/// leaves blank, so this rests on the widely-agreed 100-400 kHz identification band. Treated as a
/// ceiling, not a target.
pub const INIT_CLOCK_HZ_MAX: u32 = 400_000;

/// Clock cycles the host must supply before the first command, after the supply has been stable
/// for at least [`POWER_UP_SETTLE_MS`]. The card may spend them preparing itself.
pub const INIT_CLOCKS_MIN: u32 = 74;

/// Milliseconds the supply must be stable before the card is clocked.
pub const POWER_UP_SETTLE_MS: u32 = 1;

/// Bytes in a card's CSD register.
pub const CSD_LEN: usize = 16;

/// Bytes in one addressable sector, on every card this crate speaks to.
pub const SECTOR_LEN: usize = 512;

/// Why a CSD register could not be turned into a capacity.
///
/// Three distinct refusals rather than one, because they send a reader to three different places
/// and an opaque failure here previously sent them to none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsdError {
    /// The `CSD_STRUCTURE` field holds a value the family does not define. Carries what was read.
    UnknownStructure(u8),
    /// `READ_BL_LEN` claims a block smaller than a sector, which no real card does and which would
    /// make the capacity shift negative.
    BlockSmallerThanSector,
    /// A high-capacity MMC whose size does not fit these fields and which writes a sentinel
    /// instead. **The real figure lives in EXT_CSD, which arrives as a data block rather than a
    /// response, and this decode cannot reach it** -- so the answer is a refusal rather than a
    /// confident 4 GB for a card of any larger size.
    CapacityBeyondEncoding,
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
pub fn csd_sector_count(csd: &[u8; CSD_LEN], is_mmc: bool) -> Result<u64, CsdError> {
    if is_mmc {
        return match csd[0] >> 6 {
            0b00 | 0b01 | 0b10 => csd_exponent_sector_count(csd),
            other => Err(CsdError::UnknownStructure(other)),
        };
    }
    match csd[0] >> 6 {
        0b00 => csd_exponent_sector_count(csd),
        0b01 => {
            let c_size =
                (u64::from(csd[7] & 0x3F) << 16) | (u64::from(csd[8]) << 8) | u64::from(csd[9]);
            Ok((c_size + 1) * 1024)
        }
        other => Err(CsdError::UnknownStructure(other)),
    }
}

/// The exponent-and-multiplier encoding, shared by SD's v1 layout and by every MMC layout.
fn csd_exponent_sector_count(csd: &[u8; CSD_LEN]) -> Result<u64, CsdError> {
    let c_size =
        (u64::from(csd[6] & 0x03) << 10) | (u64::from(csd[7]) << 2) | (u64::from(csd[8]) >> 6);
    let c_size_mult = (u64::from(csd[9] & 0x03) << 1) | (u64::from(csd[10]) >> 7);
    let read_bl_len = u64::from(csd[5] & 0x0F);
    if read_bl_len < 9 {
        return Err(CsdError::BlockSmallerThanSector);
    }
    if c_size == 0xFFF {
        return Err(CsdError::CapacityBeyondEncoding);
    }
    Ok((c_size + 1) << (c_size_mult + 2 + read_bl_len - 9))
}

/// The `CMD6` function-switch encoding: what a card offers, and what it granted.
pub mod switch {
    /// The 512-bit function status a card returns to [`super::cmd::SWITCH_FUNC`], in either mode.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_v2_sd_csd_decodes_to_the_capacity_the_card_reported() {
        let csd: [u8; CSD_LEN] = [
            0x40, 0x0e, 0x00, 0x32, 0xdb, 0x79, 0x00, 0x00, 0xed, 0xc8, 0x7f, 0x80, 0x0a, 0x40,
            0x40, 0xe5,
        ];
        assert_eq!(csd_sector_count(&csd, false), Ok(62_333_952));
    }

    #[test]
    fn structure_one_means_different_arithmetic_on_the_two_families() {
        let mut csd = [0u8; CSD_LEN];
        csd[0] = 0b0100_0000;
        csd[5] = 9;
        csd[9] = 0x02;
        let as_sd = csd_sector_count(&csd, false);
        let as_mmc = csd_sector_count(&csd, true);
        assert_ne!(as_sd, as_mmc, "one CSD must not decode the same way for both families");
        assert!(as_sd.is_ok() && as_mmc.is_ok());
    }

    #[test]
    fn an_undefined_sd_structure_is_refused_by_name() {
        let mut csd = [0u8; CSD_LEN];
        csd[0] = 0b1100_0000;
        assert_eq!(csd_sector_count(&csd, false), Err(CsdError::UnknownStructure(0b11)));
    }

    #[test]
    fn the_oversized_mmc_sentinel_is_refused_rather_than_computed() {
        let mut csd = [0u8; CSD_LEN];
        csd[5] = 9;
        csd[6] = 0x03;
        csd[7] = 0xFF;
        csd[8] = 0xC0;
        assert_eq!(csd_sector_count(&csd, true), Err(CsdError::CapacityBeyondEncoding));
    }

    #[test]
    fn a_block_smaller_than_a_sector_is_refused_rather_than_shifted_negative() {
        let mut csd = [0u8; CSD_LEN];
        csd[5] = 8;
        assert_eq!(csd_sector_count(&csd, false), Err(CsdError::BlockSmallerThanSector));
    }

    #[test]
    fn the_switch_argument_differs_only_in_its_mode_bit() {
        let check = switch::arg(false, switch::GROUP_ACCESS_MODE, switch::FUNCTION_HIGH_SPEED);
        let set = switch::arg(true, switch::GROUP_ACCESS_MODE, switch::FUNCTION_HIGH_SPEED);
        assert_eq!(check, 0x00FF_FFF1);
        assert_eq!(set ^ check, 1 << 31);
    }

    #[test]
    fn the_group_one_bitmap_two_real_cards_returned_reports_no_high_speed() {
        let mut status = [0u8; switch::STATUS_LEN];
        status[12] = 0x80;
        status[13] = 0x01;
        status[16] = 0x0F;
        assert!(switch::supports(&status, switch::GROUP_ACCESS_MODE, 0));
        assert!(!switch::supports(&status, switch::GROUP_ACCESS_MODE, switch::FUNCTION_HIGH_SPEED));
        assert_eq!(switch::selected(&status, switch::GROUP_ACCESS_MODE), switch::NO_INFLUENCE);
    }

    #[test]
    fn a_card_offering_high_speed_reads_as_offering_it() {
        let mut status = [0u8; switch::STATUS_LEN];
        status[13] = 0b11;
        status[16] = switch::FUNCTION_HIGH_SPEED;
        assert!(switch::supports(&status, switch::GROUP_ACCESS_MODE, switch::FUNCTION_HIGH_SPEED));
        assert_eq!(
            switch::selected(&status, switch::GROUP_ACCESS_MODE),
            switch::FUNCTION_HIGH_SPEED
        );
    }

    #[test]
    fn every_group_reads_its_own_result_nibble() {
        let mut status = [0u8; switch::STATUS_LEN];
        status[14] = 0x65;
        status[15] = 0x43;
        status[16] = 0x21;
        for (group, expected) in (1..=6).zip([1, 2, 3, 4, 5, 6]) {
            assert_eq!(switch::selected(&status, group), expected, "group {group}");
        }
    }
}
