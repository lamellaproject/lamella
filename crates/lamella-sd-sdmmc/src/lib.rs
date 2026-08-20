//! A memory card driven over a NATIVE SD controller, on four data lines.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use lamella_sd_core::{
    ACMD41_HCS, ACMD41_VOLTAGE_WINDOW, BUS_WIDTH_4BIT, DEFAULT_SPEED_CLOCK_HZ, CSD_LEN, CsdError, INIT_CLOCK_HZ_MAX, OCR_BUSY_COMPLETE, OCR_CCS,
    POWER_UP_SETTLE_MS, SECTOR_LEN, SEND_IF_COND_ARG, SEND_IF_COND_CHECK_PATTERN, cmd,
    csd_sector_count, switch,
};

#[cfg(any(test, feature = "sim"))]
pub mod sim;

/// What kind of answer a command expects, which the controller needs BEFORE it sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    /// No response at all (CMD0). The host reports the command as sent and nothing more.
    None,
    /// A 32-bit response protected by CRC7 -- R1, R6, R7.
    Short,
    /// **A 32-bit response with NO CRC -- R3, the OCR.**
    ///
    /// The card does not compute a CRC for it and sends ones in that field, so a controller that
    /// checks will raise a CRC failure on a perfectly good response. **This is the single most
    /// common way an otherwise-correct initialization ladder fails**, and it fails at ACMD41 --
    /// the command in the polling loop -- so it reads as "the card never became ready" rather than
    /// as a decoding mistake. A host MUST accept this response with the CRC result ignored.
    ShortNoCrc,
    /// A 128-bit response -- R2, the CID or the CSD.
    Long,
}

/// A card's answer to one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// Nothing was expected and the command went out.
    None,
    /// The 32 bits of a short response.
    Short(u32),
    /// The 128 bits of a long response, most-significant word first.
    Long([u32; 4]),
}

impl Response {
    /// The short-response payload, or `None` if this was not a short response.
    #[must_use]
    pub fn short(self) -> Option<u32> {
        match self {
            Response::Short(value) => Some(value),
            _ => None,
        }
    }

    /// The long-response payload, or `None` if this was not a long response.
    #[must_use]
    pub fn long(self) -> Option<[u32; 4]> {
        match self {
            Response::Long(words) => Some(words),
            _ => None,
        }
    }
}

/// How wide the data bus is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusWidth {
    /// One data line. Every card starts here and a card that refuses the widening stays here.
    One,
    /// Four data lines -- the point of this driver.
    Four,
}

/// The host seam: a controller that can send a card command and move a data block.
///
/// Implement this over a peripheral's registers. Everything above it -- the identification ladder,
/// the four-bit negotiation, the address arithmetic, the transfer sequencing -- is written once in
/// [`SdCard`] and does not know what the controller is.
pub trait SdmmcHost {
    /// The host's own error type.
    type Error: core::fmt::Debug;

    /// Sends `index` with `arg` and waits for the answer `kind` describes.
    ///
    /// **A response timeout is a legitimate answer and not a fault** for the probing commands:
    /// CMD8 times out on a v1 card and ACMD41 is polled. A host reports it as
    /// [`SdmmcError::NoResponse`] so the ladder can act on it rather than aborting.
    fn command(
        &mut self,
        index: u8,
        arg: u32,
        kind: ResponseKind,
    ) -> Result<Response, SdmmcError<Self::Error>>;

    /// Issues `index` with `arg` AND receives the data it makes the card send.
    ///
    /// **One call rather than a command followed by a read, because the ORDER of those two is the
    /// host's business and not the same on every controller.** An STM32 SDMMC arms its data path
    /// with a `DTEN` write and the card begins sending as soon as it has decoded the command, so a
    /// host that sends first and arms second loses the leading bytes -- and loses them as a data
    /// timeout, which reads as a card that never answered. Splitting the two here would have put
    /// that race in the portable half where no host could fix it.
    ///
    /// `buf` is always a whole number of sectors, except for the 64-byte `CMD6` status.
    fn read_blocks(
        &mut self,
        index: u8,
        arg: u32,
        buf: &mut [u8],
    ) -> Result<(), SdmmcError<Self::Error>>;

    /// Issues `index` with `arg` and sends `buf` to the card.
    fn write_blocks(
        &mut self,
        index: u8,
        arg: u32,
        buf: &[u8],
    ) -> Result<(), SdmmcError<Self::Error>>;

    /// Drives the data bus at this width from now on.
    ///
    /// Called only AFTER the card has accepted the same change, and the order is not negotiable:
    /// a host that widens first is talking on lines the card is not driving.
    fn set_bus_width(&mut self, width: BusWidth);

    /// Reprograms the card clock. Called with a rate inside [`INIT_CLOCK_HZ_MAX`] during
    /// identification and with the working rate afterwards.
    fn set_clock_hz(&mut self, hz: u32);

    /// Sleeps at least `ms` milliseconds.
    fn delay_ms(&mut self, ms: u32);
}

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdmmcError<E> {
    /// The controller reported a failure of its own.
    Host(E),
    /// The card did not answer within the host's timeout. **Not always a fault** -- see
    /// [`SdmmcHost::command`].
    NoResponse,
    /// A response arrived but its CRC did not check.
    BadCrc,
    /// No card answered the reset.
    NoCard,
    /// The card answered [`cmd::SEND_IF_COND`] without echoing the check pattern, so it did not
    /// understand the command and cannot be trusted about anything else.
    CheckPatternMismatch,
    /// The card never left its initialization state within the poll budget.
    ///
    /// Carries the LAST OCR seen, because the two ways to reach here look identical otherwise: a
    /// card that is genuinely busy answers with the busy bit clear and a plausible voltage window,
    /// while a host asking the wrong question gets a reply that never changes.
    InitTimeout(u32),
    /// The card reported an error in a status response. Carries the raw R1.
    CardStatus(u32),
    /// The CSD could not be decoded into a capacity.
    Csd(CsdError),
    /// A transfer length was not a whole number of sectors, or was zero.
    BadLength,
    /// The requested range runs past the end of the card.
    OutOfRange,
}

impl<E> From<CsdError> for SdmmcError<E> {
    fn from(error: CsdError) -> Self {
        SdmmcError::Csd(error)
    }
}

/// How many times [`cmd::APP_SEND_OP_COND`] is polled before the card is called dead.
///
/// The specification allows a card one second to initialize. This is a COUNT rather than a clock
/// because the ladder has no clock; a host's `delay_ms` between polls sets the real budget.
pub const INIT_POLL_LIMIT: u32 = 1000;

/// Bits in an R1 card status that mean the card is reporting a problem.
///
/// The status is mostly informational -- current state, ready-for-data -- and only the error half
/// is worth refusing on. Checking the whole word would refuse every healthy card.
pub const R1_ERROR_MASK: u32 = 0xFDF9_E008;

/// A card on a native SD controller.
#[derive(Debug)]
pub struct SdCard<H: SdmmcHost> {
    host: H,
    /// The card's own address, assigned during identification and carried in the top half of
    /// every addressed command's argument.
    rca: u16,
    /// Whether the card is block-addressed. A standard-capacity card takes a BYTE address, so
    /// getting this backwards reads the right sector number from the wrong place entirely.
    high_capacity: bool,
    sectors: u64,
    width: BusWidth,
    /// See [`cid`](SdCard::cid).
    cid: [u8; CSD_LEN],
}

impl<H: SdmmcHost> SdCard<H> {
    /// Runs the identification ladder and leaves the card selected, four bits wide where it agreed.
    ///
    /// The order below is the protocol's, and every step depends on the one before it:
    ///
    /// | step | why it is here |
    /// |------|----------------|
    /// | CMD0 | reset to a known state |
    /// | CMD8 | v2 probe; the echo proves the card understood it |
    /// | ACMD41 | poll until ready, carrying the capacity request |
    /// | CMD2 | every card answers with its CID -- identification begins |
    /// | CMD3 | the card proposes the address the host will use |
    /// | CMD9 | the CSD, which carries the capacity |
    /// | CMD7 | select, moving the card into the transfer state |
    /// | ACMD6 | widen to four bits, card first and host second |
    ///
    /// **CMD9 comes before CMD7 deliberately.** The CSD is readable in the stand-by state and a
    /// selected card answers a different set of commands; asking in the wrong state is refused by
    /// a card that is behaving correctly, which looks like a broken card.
    pub fn init(mut host: H) -> Result<Self, SdmmcError<H::Error>> {
        host.set_clock_hz(INIT_CLOCK_HZ_MAX);
        host.set_bus_width(BusWidth::One);
        host.delay_ms(POWER_UP_SETTLE_MS);

        host.command(cmd::GO_IDLE_STATE, 0, ResponseKind::None)?;

        let v2 = match host.command(cmd::SEND_IF_COND, SEND_IF_COND_ARG, ResponseKind::Short) {
            Ok(response) => {
                let echoed = response.short().unwrap_or(0);
                if echoed as u8 != SEND_IF_COND_CHECK_PATTERN {
                    return Err(SdmmcError::CheckPatternMismatch);
                }
                true
            }
            Err(SdmmcError::NoResponse) => false,
            Err(other) => return Err(other),
        };

        let ocr = Self::poll_until_ready(&mut host, v2)?;
        let high_capacity = ocr & OCR_CCS != 0;

        let cid = csd_bytes(
            host.command(cmd::ALL_SEND_CID, 0, ResponseKind::Long)?
                .long()
                .ok_or(SdmmcError::NoResponse)?,
        );

        let published = host
            .command(cmd::SEND_RELATIVE_ADDR, 0, ResponseKind::Short)?
            .short()
            .ok_or(SdmmcError::NoResponse)?;
        let rca = (published >> 16) as u16;

        let csd = host.command(cmd::SEND_CSD, u32::from(rca) << 16, ResponseKind::Long)?;
        let sectors = csd_sector_count(&csd_bytes(csd.long().ok_or(SdmmcError::NoResponse)?), false)?;

        Self::check_status(host.command(
            cmd::SELECT_CARD,
            u32::from(rca) << 16,
            ResponseKind::Short,
        )?)?;

        let mut card =
            SdCard { host, rca, high_capacity, sectors, width: BusWidth::One, cid };
        card.widen()?;
        card.host.set_clock_hz(DEFAULT_SPEED_CLOCK_HZ);
        Ok(card)
    }

    /// Polls `ACMD41` until the card reports initialization complete, and returns its OCR.
    fn poll_until_ready(host: &mut H, v2: bool) -> Result<u32, SdmmcError<H::Error>> {
        let arg = ACMD41_VOLTAGE_WINDOW | if v2 { ACMD41_HCS } else { 0 };
        let mut last = 0u32;
        for _ in 0..INIT_POLL_LIMIT {
            host.command(cmd::APP_CMD, 0, ResponseKind::Short)?;
            let ocr = host
                .command(cmd::APP_SEND_OP_COND, arg, ResponseKind::ShortNoCrc)?
                .short()
                .ok_or(SdmmcError::NoResponse)?;
            if ocr & OCR_BUSY_COMPLETE != 0 {
                return Ok(ocr);
            }
            last = ocr;
            host.delay_ms(1);
        }
        Err(SdmmcError::InitTimeout(last))
    }

    /// Asks the card to accept four data lines and, only if it does, widens the host to match.
    ///
    /// A card that refuses is left at one bit and is still perfectly usable -- half the throughput
    /// is a far better outcome than a host driving lines the card is not.
    fn widen(&mut self) -> Result<(), SdmmcError<H::Error>> {
        self.host.command(cmd::APP_CMD, u32::from(self.rca) << 16, ResponseKind::Short)?;
        let status = self.host.command(
            cmd::APP_SET_BUS_WIDTH,
            BUS_WIDTH_4BIT,
            ResponseKind::Short,
        )?;
        if Self::check_status(status).is_ok() {
            self.host.set_bus_width(BusWidth::Four);
            self.width = BusWidth::Four;
        }
        Ok(())
    }

    /// Asks the card for High Speed and raises the clock only if it grants it.
    ///
    /// **Unlike the SPI path, this is a real lever here.** The access-mode group belongs to the SD
    /// bus, so a card that reports the function unsupported over SPI can still grant it natively.
    ///
    /// Returns whether the switch happened. A refusal is not a fault.
    pub fn try_high_speed(&mut self) -> Result<bool, SdmmcError<H::Error>> {
        let arg = switch::arg(true, switch::GROUP_ACCESS_MODE, switch::FUNCTION_HIGH_SPEED);
        let mut status = [0u8; switch::STATUS_LEN];
        self.host.read_blocks(cmd::SWITCH_FUNC, arg, &mut status)?;
        let granted =
            switch::selected(&status, switch::GROUP_ACCESS_MODE) == switch::FUNCTION_HIGH_SPEED;
        if granted {
            self.host.set_clock_hz(switch::HIGH_SPEED_CLOCK_HZ);
        }
        Ok(granted)
    }

    /// Reprograms the bus clock on an already-identified card.
    ///
    /// A working rate is a BOARD decision within the card's ceiling -- signal integrity, trace
    /// length and pull-ups all bound it, and none of those are things the card or this driver
    /// knows. [`init`](Self::init) leaves a conservative default-speed rate; a board that has
    /// measured its own limit sets it here.
    pub fn set_clock_hz(&mut self, hz: u32) {
        self.host.set_clock_hz(hz);
    }

    /// The card's CID register, which is the only field that distinguishes two identical cards.
    ///
    /// **Held for identification, not for decoding.** Its fields sit at different offsets for SD
    /// and MMC, so a caller that wants an identity should fold the whole register rather than read
    /// a serial-number field out of it -- a value that quietly meant something different per card
    /// family would be worse than none.
    #[must_use]
    pub fn cid(&self) -> &[u8; CSD_LEN] {
        &self.cid
    }

    /// How many 512-byte sectors the card holds.
    #[must_use]
    pub fn sector_count(&self) -> u64 {
        self.sectors
    }

    /// How wide the bus ended up. `One` means the card declined the widening.
    #[must_use]
    pub fn bus_width(&self) -> BusWidth {
        self.width
    }

    /// Whether the card is block-addressed.
    #[must_use]
    pub fn is_high_capacity(&self) -> bool {
        self.high_capacity
    }

    /// Reads `buf.len() / 512` consecutive sectors starting at `lba`.
    pub fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), SdmmcError<H::Error>> {
        let blocks = self.check_range(lba, buf.len())?;
        let index =
            if blocks == 1 { cmd::READ_SINGLE_BLOCK } else { cmd::READ_MULTIPLE_BLOCK };
        if let Err(error) = self.host.read_blocks(index, self.address(lba), buf) {
            let _ = self.host.command(cmd::STOP_TRANSMISSION, 0, ResponseKind::Short);
            return Err(error);
        }
        if blocks > 1 {
            self.host.command(cmd::STOP_TRANSMISSION, 0, ResponseKind::Short)?;
        }
        Ok(())
    }

    /// Writes `buf.len() / 512` consecutive sectors starting at `lba`.
    pub fn write_sectors(&mut self, lba: u64, buf: &[u8]) -> Result<(), SdmmcError<H::Error>> {
        let blocks = self.check_range(lba, buf.len())?;
        let index = if blocks == 1 { cmd::WRITE_BLOCK } else { cmd::WRITE_MULTIPLE_BLOCK };
        self.host.write_blocks(index, self.address(lba), buf)?;
        if blocks > 1 {
            self.host.command(cmd::STOP_TRANSMISSION, 0, ResponseKind::Short)?;
        }
        self.wait_until_ready()
    }

    /// Polls the card's own status until it reports itself out of the programming state.
    fn wait_until_ready(&mut self) -> Result<(), SdmmcError<H::Error>> {
        let mut last = 0u32;
        for _ in 0..INIT_POLL_LIMIT {
            let status = self
                .host
                .command(cmd::SEND_STATUS, u32::from(self.rca) << 16, ResponseKind::Short)?
                .short()
                .ok_or(SdmmcError::NoResponse)?;
            if status & R1_ERROR_MASK != 0 {
                return Err(SdmmcError::CardStatus(status));
            }
            if status & (1 << 8) != 0 {
                return Ok(());
            }
            last = status;
            self.host.delay_ms(1);
        }
        Err(SdmmcError::InitTimeout(last))
    }

    /// The address a data command carries: a SECTOR number on a high-capacity card and a BYTE
    /// offset on a standard-capacity one.
    fn address(&self, lba: u64) -> u32 {
        if self.high_capacity { lba as u32 } else { (lba * SECTOR_LEN as u64) as u32 }
    }

    /// Validates a transfer's length and range, returning the block count.
    fn check_range(&self, lba: u64, len: usize) -> Result<usize, SdmmcError<H::Error>> {
        if len == 0 || len % SECTOR_LEN != 0 {
            return Err(SdmmcError::BadLength);
        }
        let blocks = len / SECTOR_LEN;
        if lba.saturating_add(blocks as u64) > self.sectors {
            return Err(SdmmcError::OutOfRange);
        }
        Ok(blocks)
    }

    /// Refuses an R1 that reports an error, and passes anything else through.
    fn check_status(response: Response) -> Result<(), SdmmcError<H::Error>> {
        match response.short() {
            Some(status) if status & R1_ERROR_MASK != 0 => Err(SdmmcError::CardStatus(status)),
            _ => Ok(()),
        }
    }

    /// Gives the host back.
    pub fn release(self) -> H {
        self.host
    }

    /// The host beneath this card, for a board that has to reconfigure its own controller.
    ///
    /// [`release`](Self::release) already hands the host back, but only by consuming the card --
    /// so a board that wants to change something about its controller and then keep reading has to
    /// re-identify the card to get one, which is a card-protocol cost paid for a host-side change.
    /// **Nothing reachable through here is part of the card protocol**; this seam exists so that
    /// host facts, such as which DMA stream serves the controller, stay on the host side of it.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }
}

/// The 16 CSD bytes a long response carries.
///
/// **A controller drops the CSD's low byte.** The register is 128 bits including a CRC and a stop
/// bit that the host strips, so the four response words hold bits 127:8 left-aligned -- the decode
/// wants the register's own byte order with a zero in the last position. Getting this wrong shifts
/// every field by eight bits and yields a capacity that is wrong by a large power of two, which is
/// the kind of wrong that still looks like a number.
fn csd_bytes(words: [u32; 4]) -> [u8; CSD_LEN] {
    let mut out = [0u8; CSD_LEN];
    for (i, word) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimCard;

    fn card() -> SdCard<SimCard> {
        SdCard::init(SimCard::sdhc()).expect("a healthy SDHC card initializes")
    }

    #[test]
    fn identification_leaves_the_bus_at_a_working_clock_not_the_identification_one() {
        let card = card();
        let clock = card.release().clock_hz;
        assert!(
            clock > lamella_sd_core::INIT_CLOCK_HZ_MAX,
            "the bus was left at the identification rate ({clock} Hz); transfers would be correct              and about sixty times too slow"
        );
        assert_eq!(clock, lamella_sd_core::DEFAULT_SPEED_CLOCK_HZ);
    }

    #[test]
    fn a_healthy_card_ends_up_four_bits_wide() {
        let card = card();
        assert_eq!(card.bus_width(), BusWidth::Four, "the whole point of this driver");
        assert!(card.is_high_capacity());
    }

    #[test]
    fn a_card_that_refuses_four_bits_is_left_at_one_and_still_reads() {
        let mut card = SdCard::init(SimCard::sdhc().refusing_wide_bus())
            .expect("a card that declines the widening still initializes");
        assert_eq!(card.bus_width(), BusWidth::One);
        let mut buf = [0u8; SECTOR_LEN];
        assert!(card.read_sectors(0, &mut buf).is_ok());
    }

    #[test]
    fn the_identification_ladder_runs_in_the_order_the_protocol_requires() {
        let card = card();
        let log = card.release().command_log;
        let expected = [
            cmd::GO_IDLE_STATE,
            cmd::SEND_IF_COND,
            cmd::APP_CMD,
            cmd::APP_SEND_OP_COND,
            cmd::APP_CMD,
            cmd::APP_SEND_OP_COND,
            cmd::APP_CMD,
            cmd::APP_SEND_OP_COND,
            cmd::APP_CMD,
            cmd::APP_SEND_OP_COND,
            cmd::ALL_SEND_CID,
            cmd::SEND_RELATIVE_ADDR,
            cmd::SEND_CSD,
            cmd::SELECT_CARD,
            cmd::APP_CMD,
            cmd::APP_SET_BUS_WIDTH,
        ];
        assert_eq!(log.len(), expected.len(), "the ladder sent a different number of commands");
        for (position, wanted) in expected.iter().enumerate() {
            let sent = log.iter().nth(position).expect("logged").0;
            assert_eq!(sent, *wanted, "at position {position}");
        }
    }

    #[test]
    fn the_ocr_is_requested_as_an_unchecked_response() {
        let card = card();
        let log = card.release().command_log;
        let ocr_kind = log
            .iter()
            .find(|entry| entry.0 == cmd::APP_SEND_OP_COND)
            .map(|entry| entry.2)
            .expect("ACMD41 was sent");
        assert_eq!(ocr_kind, ResponseKind::ShortNoCrc);
    }

    #[test]
    fn the_initialization_command_advertises_a_voltage_window() {
        let card = card();
        let log = card.release().command_log;
        let arg = log
            .iter()
            .find(|entry| entry.0 == cmd::APP_SEND_OP_COND)
            .map(|entry| entry.1)
            .expect("ACMD41 was sent");
        assert_ne!(
            arg & lamella_sd_core::ACMD41_VOLTAGE_WINDOW,
            0,
            "ACMD41 with a zero voltage window is an inquiry and never initializes a card"
        );
    }

    #[test]
    fn a_v1_card_that_never_answers_cmd8_still_initializes() {
        let card = SdCard::init(SimCard::sdsc()).expect("a v1 card initializes");
        assert!(!card.is_high_capacity(), "a v1 card is byte-addressed");
        assert_eq!(card.sector_count(), SimCard::SDSC_SECTORS);
    }

    #[test]
    fn a_standard_capacity_card_is_addressed_in_bytes_and_a_high_capacity_one_in_sectors() {
        let mut sdhc = card();
        let mut buf = [0u8; SECTOR_LEN];
        sdhc.read_sectors(2, &mut buf).unwrap();
        let sdhc_arg = last_read_arg(sdhc.release());

        let mut sdsc = SdCard::init(SimCard::sdsc()).unwrap();
        sdsc.read_sectors(2, &mut buf).unwrap();
        let sdsc_arg = last_read_arg(sdsc.release());

        assert_eq!(sdhc_arg, 2);
        assert_eq!(sdsc_arg, 2 * SECTOR_LEN as u32);
    }

    #[test]
    fn a_read_past_the_end_is_refused_rather_than_wrapped() {
        let mut card = card();
        let mut buf = [0u8; SECTOR_LEN];
        let last = card.sector_count();
        assert_eq!(card.read_sectors(last, &mut buf), Err(SdmmcError::OutOfRange));
    }

    #[test]
    fn a_partial_sector_is_refused() {
        let mut card = card();
        let mut buf = [0u8; 100];
        assert_eq!(card.read_sectors(0, &mut buf), Err(SdmmcError::BadLength));
        assert_eq!(card.read_sectors(0, &mut []), Err(SdmmcError::BadLength));
    }

    #[test]
    fn a_multi_block_read_is_terminated_by_a_stop() {
        let mut card = card();
        let mut buf = [0u8; SECTOR_LEN * 4];
        card.read_sectors(0, &mut buf).unwrap();
        let log = card.release().command_log;
        assert!(log.iter().any(|entry| entry.0 == cmd::STOP_TRANSMISSION));
    }

    #[test]
    fn a_single_block_read_is_not_terminated_by_a_stop() {
        let mut card = card();
        let mut buf = [0u8; SECTOR_LEN];
        card.read_sectors(0, &mut buf).unwrap();
        let log = card.release().command_log;
        assert!(!log.iter().any(|entry| entry.0 == cmd::STOP_TRANSMISSION));
    }

    #[test]
    fn high_speed_is_granted_by_a_card_that_offers_it_and_raises_the_clock() {
        let mut card = card();
        assert!(card.try_high_speed().unwrap());
        assert_eq!(card.release().clock_hz, switch::HIGH_SPEED_CLOCK_HZ);
    }

    #[test]
    fn a_card_declining_high_speed_leaves_the_clock_where_it_was() {
        let mut card = SdCard::init(SimCard::sdhc().refusing_high_speed()).unwrap();
        let before = card.host.clock_hz;
        assert!(!card.try_high_speed().unwrap());
        assert_eq!(card.release().clock_hz, before);
    }

    #[test]
    fn a_card_that_never_answers_the_reset_is_reported_as_such() {
        assert!(matches!(
            SdCard::init(SimCard::sdhc().dead()),
            Err(SdmmcError::NoResponse | SdmmcError::NoCard)
        ));
    }

    #[test]
    fn a_wrong_check_pattern_is_refused_rather_than_ignored() {
        assert_eq!(
            SdCard::init(SimCard::sdhc().with_bad_check_pattern()).unwrap_err(),
            SdmmcError::CheckPatternMismatch
        );
    }

    #[test]
    fn the_csd_words_decode_to_the_capacity_the_card_advertises() {
        assert_eq!(card().sector_count(), SimCard::SDHC_SECTORS);
    }

    fn last_read_arg(host: SimCard) -> u32 {
        host.command_log
            .iter()
            .rev()
            .find(|entry| entry.0 == cmd::READ_SINGLE_BLOCK || entry.0 == cmd::READ_MULTIPLE_BLOCK)
            .map(|entry| entry.1)
            .expect("a read was issued")
    }
}
