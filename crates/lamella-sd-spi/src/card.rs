//! The SPI-mode initialization ladder: bring a card up, and in doing so decide WHAT it is.

use crate::{
    cmd, command_frame, csd_sector_count, r1, switch, token, SdSpiBus, ACMD41_HCS,
    COMMAND_FRAME_LEN,
    INIT_CLOCK_HZ_MAX,
    OCR_CCS, POWER_UP_SETTLE_MS, SEND_IF_COND_ARG, SEND_IF_COND_CHECK_PATTERN,
};
#[cfg(feature = "write")]
use crate::data_response;
use lamella_cil_runtime::block::{
    transfer_sectors, BlockDevice, BlockError, BlockResult, SECTOR_SIZE,
};

/// How many times `CMD0` is retried before a silent bus is declared empty. A present card answers
/// the first or second reset; the budget covers a slow power-up.
const RESET_RETRIES: u32 = 16;

/// How many times `ACMD41` / `CMD1` are polled before the card is declared stuck. With the 10 ms
/// inter-poll delay this is a ~2.5 s budget -- comfortably over the ~1 s the specification allows a
/// card to take leaving idle.
const OP_COND_POLL_LIMIT: u32 = 256;

/// Milliseconds between `ACMD41` / `CMD1` polls.
const OP_COND_POLL_DELAY_MS: u32 = 10;

/// The clock the driver requests once identification is done. 25 MHz is the SD SPI-mode ceiling;
/// the bus clamps the request to the fastest rate its own divisor can produce, so the effective
/// clock is "as fast as the card's spec allows, capped by what the wiring can drive".
const WORKING_CLOCK_HZ: u32 = 25_000_000;

/// What kind of card [`SdCard::init`] found. This fixes two things the rest of the driver depends
/// on: the addressing mode (byte vs sector) and the protocol family (SD vs MMC).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CardType {
    /// Standard-capacity SD, version 1 -- byte-addressed, <= 2 GB.
    Sd1,
    /// Standard-capacity SD, version 2 -- byte-addressed, <= 2 GB.
    Sd2,
    /// High- or extended-capacity SD (SDHC/SDXC) -- SECTOR-addressed.
    Sdhc,
    /// A MultiMediaCard -- byte-addressed for the <= 2 GB parts this targets. (High-capacity eMMC
    /// sector addressing is a later concern; see [`SdCard::init`].)
    Mmc,
}

impl CardType {
    /// Whether the card is addressed in 512-byte SECTORS (an LBA) rather than in bytes. Only
    /// high-capacity SD is; every other family here is byte-addressed and multiplies the LBA by
    /// [`SECTOR_SIZE`] to form the command argument.
    #[must_use]
    pub fn block_addressed(self) -> bool {
        matches!(self, CardType::Sdhc)
    }

    /// Whether this is a MultiMediaCard rather than an SD card -- the distinction the init ladder
    /// exists to make correctly.
    #[must_use]
    pub fn is_mmc(self) -> bool {
        matches!(self, CardType::Mmc)
    }
}

/// Why [`SdCard::init`] could not bring a card up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError<E> {
    /// The SPI bus itself failed.
    Bus(E),
    /// No card answered the reset within the retry budget -- an empty slot, or no card power.
    NoCard,
    /// A card answered `CMD8` but echoed the wrong check pattern -- it did not understand the
    /// command, so it is not a card this driver will trust.
    BadInterfaceCondition,
    /// The card acknowledged initialization but never left its idle state within the budget. An
    /// SDUC card reaches this deliberately -- it stays busy to signal it cannot run SPI mode.
    InitTimeout,
    /// The card is neither SD nor MMC (it refused both `ACMD41` and `CMD1`).
    UnsupportedCard,
    /// A command returned an unexpected error status where a clean one was required.
    Protocol,
}

/// One memory card reached over an [`SdSpiBus`], brought up and identified. Owns the bus and
/// implements [`BlockDevice`]; [`card_type`](Self::card_type) reports what it is.
pub struct SdCard<B: SdSpiBus> {
    bus: B,
    card_type: CardType,
    /// The sector count, read from the CSD on first request and cached (a CSD does not change).
    cached_sector_count: Option<u64>,
}

impl<B: SdSpiBus> core::fmt::Debug for SdCard<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SdCard").field("card_type", &self.card_type).finish_non_exhaustive()
    }
}

impl<B: SdSpiBus> SdCard<B> {
    /// Runs the SPI-mode initialization ladder on `bus` and returns the identified card, or the
    /// reason it could not be brought up.
    ///
    /// MMC is handled by the same call: once `CMD8` has ruled out SD v2 and `ACMD41` is refused,
    /// the card is driven ready by its native `CMD1`. The <= 2 GB MMC parts this targets are
    /// byte-addressed, so `CMD1` is sent with a zero argument (which those cards accept);
    /// high-capacity eMMC would need a sector-mode + voltage-window argument, which this driver
    /// does not implement.
    pub fn init(bus: B) -> Result<Self, InitError<B::Error>> {
        let mut card = SdCard { bus, card_type: CardType::Sd1, cached_sector_count: None };
        card.run_init()?;
        Ok(card)
    }

    /// What kind of card this is.
    #[must_use]
    pub fn card_type(&self) -> CardType {
        self.card_type
    }

    /// Reclaims the underlying bus (e.g. to hand it to another card, or to power down).
    pub fn release_bus(self) -> B {
        self.bus
    }

    fn run_init(&mut self) -> Result<(), InitError<B::Error>> {
        self.bus.set_clock_hz(INIT_CLOCK_HZ_MAX);
        self.bus.set_chip_select(false);
        self.bus.delay_ms(POWER_UP_SETTLE_MS);
        for _ in 0..10 {
            self.xfer_byte(0xFF)?;
        }

        let mut in_idle = false;
        for _ in 0..RESET_RETRIES {
            let response = self.command(cmd::GO_IDLE_STATE, 0)?;
            self.release()?;
            if response == Some(r1::IDLE_STATE) {
                in_idle = true;
                break;
            }
        }
        if !in_idle {
            return Err(InitError::NoCard);
        }

        let cmd8 = self.command(cmd::SEND_IF_COND, SEND_IF_COND_ARG)?.ok_or(InitError::NoCard)?;
        let is_v2 = if cmd8 & r1::ILLEGAL_COMMAND == 0 {
            let echo = self.read_u32()?;
            self.release()?;
            if (echo & 0xFF) as u8 != SEND_IF_COND_CHECK_PATTERN {
                return Err(InitError::BadInterfaceCondition);
            }
            true
        } else {
            self.release()?;
            false
        };

        let card_type = if is_v2 {
            if !self.poll_sd_op_cond(true)? {
                return Err(InitError::Protocol);
            }
            let ocr = self.read_ocr()?;
            if ocr & OCR_CCS != 0 {
                CardType::Sdhc
            } else {
                CardType::Sd2
            }
        } else if self.poll_sd_op_cond(false)? {
            CardType::Sd1
        } else {
            self.poll_mmc_op_cond()?;
            CardType::Mmc
        };
        self.card_type = card_type;

        if !card_type.block_addressed() {
            let response =
                self.command(cmd::SET_BLOCKLEN, SECTOR_SIZE as u32)?.ok_or(InitError::NoCard)?;
            self.release()?;
            if response != 0 {
                return Err(InitError::Protocol);
            }
        }

        self.bus.set_clock_hz(WORKING_CLOCK_HZ);
        Ok(())
    }

    /// Reprograms the working bus clock (e.g. to back off for a long cable, or push a short trace
    /// harder). The bus clamps to the hardware's range. Only meaningful after [`init`](Self::init);
    /// init sets it to [`WORKING_CLOCK_HZ`] on its own.
    pub fn set_bus_speed(&mut self, hz: u32) {
        self.bus.set_clock_hz(hz);
    }

    /// Polls `ACMD41` (each preceded by `CMD55`). Returns `Ok(true)` once the card leaves idle
    /// (it IS an SD card), `Ok(false)` if `CMD55`/`ACMD41` is refused as illegal (it is NOT SD --
    /// an MMC), or [`InitError::InitTimeout`] if it is SD but never became ready.
    fn poll_sd_op_cond(&mut self, host_capacity: bool) -> Result<bool, InitError<B::Error>> {
        let arg = if host_capacity { ACMD41_HCS } else { 0 };
        for _ in 0..OP_COND_POLL_LIMIT {
            let app = self.command(cmd::APP_CMD, 0)?.ok_or(InitError::NoCard)?;
            self.release()?;
            if app & r1::ILLEGAL_COMMAND != 0 {
                return Ok(false);
            }
            let op = self.command(cmd::APP_SEND_OP_COND, arg)?.ok_or(InitError::NoCard)?;
            self.release()?;
            if op & r1::ILLEGAL_COMMAND != 0 {
                return Ok(false);
            }
            if op & r1::IDLE_STATE == 0 {
                return Ok(true);
            }
            self.bus.delay_ms(OP_COND_POLL_DELAY_MS);
        }
        Err(InitError::InitTimeout)
    }

    /// Polls the MMC native `CMD1` until the card leaves idle. [`InitError::UnsupportedCard`] if
    /// even `CMD1` is refused (neither SD nor MMC), [`InitError::InitTimeout`] if it never readies.
    fn poll_mmc_op_cond(&mut self) -> Result<(), InitError<B::Error>> {
        for _ in 0..OP_COND_POLL_LIMIT {
            let op = self.command(cmd::SEND_OP_COND, 0)?.ok_or(InitError::NoCard)?;
            self.release()?;
            if op & r1::ILLEGAL_COMMAND != 0 {
                return Err(InitError::UnsupportedCard);
            }
            if op & r1::IDLE_STATE == 0 {
                return Ok(());
            }
            self.bus.delay_ms(OP_COND_POLL_DELAY_MS);
        }
        Err(InitError::InitTimeout)
    }

    /// `CMD58`: reads the 32-bit OCR (its CCS bit selects block vs byte addressing).
    fn read_ocr(&mut self) -> Result<u32, InitError<B::Error>> {
        let response = self.command(cmd::READ_OCR, 0)?.ok_or(InitError::NoCard)?;
        if response & !r1::IDLE_STATE != 0 {
            self.release()?;
            return Err(InitError::Protocol);
        }
        let ocr = self.read_u32()?;
        self.release()?;
        Ok(ocr)
    }

    /// Sends a command frame with chip-select asserted and returns its R1 response, or `None` if
    /// the card did not respond within the command-to-response window. Chip-select is LEFT
    /// asserted so a caller can read any trailing bytes (R3/R7/data); every caller pairs this with
    /// [`release`](Self::release).
    fn command(&mut self, index: u8, arg: u32) -> Result<Option<u8>, InitError<B::Error>> {
        self.bus.set_chip_select(true);
        let mut tx = [0xFFu8; 1 + COMMAND_FRAME_LEN];
        tx[1..].copy_from_slice(&command_frame(index, arg));
        let mut rx = [0u8; 1 + COMMAND_FRAME_LEN];
        self.bus.transfer(&tx, &mut rx).map_err(InitError::Bus)?;
        self.read_r1()
    }

    /// Reads the R1 response: clock idle bytes until one arrives with its top bit clear, up to the
    /// command-to-response maximum. `None` if none does -- the card is absent or wedged.
    fn read_r1(&mut self) -> Result<Option<u8>, InitError<B::Error>> {
        for _ in 0..10 {
            let byte = self.xfer_byte(0xFF)?;
            if byte & 0x80 == 0 {
                return Ok(Some(byte));
            }
        }
        Ok(None)
    }

    /// Reads four bytes MSB-first (an R3/R7 trailer -- OCR or the CMD8 echo).
    fn read_u32(&mut self) -> Result<u32, InitError<B::Error>> {
        let mut value = 0u32;
        for _ in 0..4 {
            value = (value << 8) | u32::from(self.xfer_byte(0xFF)?);
        }
        Ok(value)
    }

    /// Deasserts chip-select and clocks eight idle cycles so the card releases the data-out line
    /// before the next transaction.
    fn release(&mut self) -> Result<(), InitError<B::Error>> {
        self.bus.set_chip_select(false);
        self.xfer_byte(0xFF)?;
        Ok(())
    }

    /// Clocks one byte, returning what the card clocked back.
    fn xfer_byte(&mut self, out: u8) -> Result<u8, InitError<B::Error>> {
        let tx = [out];
        let mut rx = [0u8];
        self.bus.transfer(&tx, &mut rx).map_err(InitError::Bus)?;
        Ok(rx[0])
    }
}

/// How many idle bytes to clock while waiting for a data start-block token (the card's access
/// time, Nac) before giving up.
const DATA_TOKEN_TRIES: u32 = 1 << 16;

/// How many idle bytes to clock while waiting out a write's programming time (the card holds the
/// line low while busy) before giving up.
const BUSY_TRIES: u32 = 1 << 18;

impl<B: SdSpiBus> SdCard<B> {
    /// The command argument for `lba`: a BYTE offset on a byte-addressed card, the sector index
    /// itself on a block-addressed (SDHC) card. THE one place the addressing mode is applied -- get
    /// it wrong and every read and write lands on the wrong sector, which is exactly the class of
    /// bug MMC-beside-SD exists to make impossible.
    fn block_arg(&self, lba: u64) -> u32 {
        if self.card_type.block_addressed() {
            lba as u32
        } else {
            (lba * SECTOR_SIZE as u64) as u32
        }
    }

    fn io_byte(&mut self, out: u8) -> BlockResult<u8> {
        let tx = [out];
        let mut rx = [0u8];
        self.bus.transfer(&tx, &mut rx).map_err(|_| BlockError::Io)?;
        Ok(rx[0])
    }

    /// Sends a command and returns its R1, leaving chip-select asserted for the data phase.
    fn io_command(&mut self, index: u8, arg: u32) -> BlockResult<u8> {
        self.bus.set_chip_select(true);
        let mut tx = [0xFFu8; 1 + COMMAND_FRAME_LEN];
        tx[1..].copy_from_slice(&command_frame(index, arg));
        let mut rx = [0u8; 1 + COMMAND_FRAME_LEN];
        self.bus.transfer(&tx, &mut rx).map_err(|_| BlockError::Io)?;
        for _ in 0..10 {
            let byte = self.io_byte(0xFF)?;
            if byte & 0x80 == 0 {
                return Ok(byte);
            }
        }
        Err(BlockError::NotReady)
    }

    fn io_release(&mut self) -> BlockResult<()> {
        self.bus.set_chip_select(false);
        self.io_byte(0xFF)?;
        Ok(())
    }

    /// Clocks idle bytes until the card stops holding the line low -- its current program cycle is
    /// complete. This is what makes a non-buffering [`flush`](BlockDevice::flush) meaningful.
    fn wait_not_busy(&mut self) -> BlockResult<()> {
        for _ in 0..BUSY_TRIES {
            if self.io_byte(0xFF)? == 0xFF {
                return Ok(());
            }
        }
        Err(BlockError::NotReady)
    }

    /// Waits for the start-block token, then reads `buf` plus the two trailing CRC bytes (CRC is
    /// off in SPI mode by default, so it is read and discarded).
    ///
    /// THE PAYLOAD MOVES IN ONE CALL, and that is a throughput decision rather than a tidiness one.
    /// A per-byte loop hands the bus one byte at a time, 512 times per sector, so every per-call cost
    /// -- the call itself, the slice bounds, the trait dispatch, and any setup the board's
    /// implementation does -- is paid 512 times instead of once, and no bus implementation can
    /// pipeline a transfer it is given one byte at a time. The token search stays byte-at-a-time
    /// because its length is not known in advance: the card decides when to answer.
    fn read_data_block(&mut self, buf: &mut [u8]) -> BlockResult<()> {
        for _ in 0..DATA_TOKEN_TRIES {
            let byte = self.io_byte(0xFF)?;
            if byte == token::START_BLOCK {
                const IDLE: [u8; SECTOR_SIZE] = [0xFF; SECTOR_SIZE];
                for chunk in buf.chunks_mut(SECTOR_SIZE) {
                    self.bus
                        .transfer(&IDLE[..chunk.len()], chunk)
                        .map_err(|_| BlockError::Io)?;
                }
                self.io_byte(0xFF)?;
                self.io_byte(0xFF)?;
                return Ok(());
            }
            if byte != 0xFF {
                return Err(BlockError::Io);
            }
        }
        Err(BlockError::NotReady)
    }

    /// Sends one data block: the start token, `buf`, a dummy CRC, then reads the data-response
    /// token (polling past the card's response latency) and waits out its programming.
    #[cfg(feature = "write")]
    fn write_data_block(&mut self, start_token: u8, buf: &[u8]) -> BlockResult<()> {
        self.io_byte(start_token)?;
        const CHUNK: usize = 64;
        let mut sink = [0u8; CHUNK];
        for chunk in buf.chunks(CHUNK) {
            self.bus
                .transfer(chunk, &mut sink[..chunk.len()])
                .map_err(|_| BlockError::Io)?;
        }
        self.io_byte(0xFF)?;
        self.io_byte(0xFF)?;
        let mut response = 0xFF;
        for _ in 0..DATA_TOKEN_TRIES {
            response = self.io_byte(0xFF)?;
            if response != 0xFF {
                break;
            }
        }
        match data_response::classify(response) {
            Some(Ok(())) => self.wait_not_busy(),
            Some(Err(rejection)) => Err(rejection.to_block_error()),
            None => Err(BlockError::Io),
        }
    }

    /// Runs `CMD6` and returns the 64-byte function status, or `None` if the card REFUSED the
    /// command -- which is what a card without command class 10 does, and is not an error.
    ///
    /// `set` picks the mode: `false` asks what the card offers and changes nothing, `true` selects
    /// the function. Both modes answer with the same status block.
    fn switch_func(
        &mut self,
        set: bool,
        group: u8,
        function: u8,
    ) -> BlockResult<Option<[u8; switch::STATUS_LEN]>> {
        let response = self.io_command(cmd::SWITCH_FUNC, switch::arg(set, group, function))?;
        if response != 0 {
            self.io_release()?;
            return Ok(None);
        }
        let mut status = [0u8; switch::STATUS_LEN];
        self.read_data_block(&mut status)?;
        self.io_release()?;
        Ok(Some(status))
    }

    /// Asks what the card offers in `group` without changing anything (`CMD6` mode 0). `None` means
    /// the card does not implement the command at all.
    ///
    /// Decode with [`switch::supports`] and [`switch::selected`].
    pub fn query_function(
        &mut self,
        group: u8,
        function: u8,
    ) -> BlockResult<Option<[u8; switch::STATUS_LEN]>> {
        self.switch_func(false, group, function)
    }

    /// Selects `function` in `group` (`CMD6` mode 1) and returns the resulting status. `None` means
    /// the card refused the command.
    ///
    /// A card that does not offer the function answers this SUCCESSFULLY, writing
    /// [`switch::NO_INFLUENCE`] into the group's result nibble rather than reporting an error. Read
    /// the result back with [`switch::selected`]: nothing else distinguishes a switch that happened
    /// from one that did not.
    pub fn switch_function(
        &mut self,
        group: u8,
        function: u8,
    ) -> BlockResult<Option<[u8; switch::STATUS_LEN]>> {
        self.switch_func(true, group, function)
    }

    /// Puts the card into High Speed and raises the requested bus clock to match, returning whether
    /// it happened.
    ///
    /// **This is the only way past the CSD's `TRAN_SPEED`.** That field reports the ceiling in
    /// force -- 25 MHz on a default-speed card and on a High Speed card that has not been asked --
    /// so a host that reads it and stops is capped at half the rate the card would take. After a
    /// successful switch the same field reads 50 MHz.
    ///
    /// Two ways to answer `false` and neither is a fault: the card has no `CMD6` at all, or it
    /// accepted the command and declined to switch.
    ///
    /// The result nibble is the only gate on the outcome, and reading it is not optional. Mode 1
    /// against an unsupported function is legal and harmless: the card answers OK, writes
    /// [`switch::NO_INFLUENCE`] and stays where it was. A driver that took the OK for the answer
    /// would raise the bus to 50 MHz against a card whose ceiling is 25, and that surfaces later as
    /// corrupt data on a link that looks in specification.
    ///
    /// No query is sent first. [`query_function`](Self::query_function) reports what the card
    /// OFFERS, which is worth reading for diagnosis and is a different fact from what it granted,
    /// but it cannot change the outcome and this does not run it.
    ///
    /// The bus CLAMPS the requested rate to what its divisor can produce, so this asks for the
    /// card's new ceiling rather than naming a frequency: what a board actually drives is the
    /// board's to decide and the card's to bound.
    ///
    /// One thing the specification allows and this does not model: a switch may take up to 100 ms,
    /// reported through the status block's busy fields, and nothing here waits for that. A card
    /// slower than the eight release clocks would fail its next command rather than corrupt
    /// anything.
    pub fn try_high_speed(&mut self) -> BlockResult<bool> {
        const GROUP: u8 = switch::GROUP_ACCESS_MODE;
        const FUNCTION: u8 = switch::FUNCTION_HIGH_SPEED;

        let switched = match self.switch_function(GROUP, FUNCTION)? {
            Some(status) => switch::selected(&status, GROUP) == FUNCTION,
            None => return Ok(false),
        };
        if switched {
            self.bus.set_clock_hz(switch::HIGH_SPEED_CLOCK_HZ);
        }
        Ok(switched)
    }
}

impl<B: SdSpiBus> BlockDevice for SdCard<B> {
    fn sector_count(&mut self) -> BlockResult<u64> {
        if let Some(count) = self.cached_sector_count {
            return Ok(count);
        }
        let response = self.io_command(cmd::SEND_CSD, 0)?;
        if response != 0 {
            self.io_release()?;
            return Err(BlockError::Io);
        }
        let mut csd = [0u8; 16];
        self.read_data_block(&mut csd)?;
        self.io_release()?;
        let count = csd_sector_count(&csd, self.card_type.is_mmc())?;
        self.cached_sector_count = Some(count);
        Ok(count)
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> BlockResult<()> {
        let total = self.sector_count()?;
        let count = transfer_sectors(buf.len(), lba, total)?;

        if count > 1 {
            let response = self.io_command(cmd::READ_MULTIPLE_BLOCK, self.block_arg(lba))?;
            if response != 0 {
                self.io_release()?;
                return Err(BlockError::Io);
            }
            for chunk in buf.chunks_mut(SECTOR_SIZE) {
                self.read_data_block(chunk)?;
            }
            self.io_command(cmd::STOP_TRANSMISSION, 0)?;
            self.wait_not_busy()?;
            self.io_release()?;
            return Ok(());
        }

        for (index, chunk) in buf.chunks_mut(SECTOR_SIZE).enumerate() {
            let arg = self.block_arg(lba + index as u64);
            let response = self.io_command(cmd::READ_SINGLE_BLOCK, arg)?;
            if response != 0 {
                self.io_release()?;
                return Err(BlockError::Io);
            }
            self.read_data_block(chunk)?;
            self.io_release()?;
        }
        Ok(())
    }

    #[cfg(feature = "write")]
    fn write_sectors(&mut self, lba: u64, buf: &[u8]) -> BlockResult<()> {
        let total = self.sector_count()?;
        transfer_sectors(buf.len(), lba, total)?;
        for (index, chunk) in buf.chunks(SECTOR_SIZE).enumerate() {
            let arg = self.block_arg(lba + index as u64);
            let response = self.io_command(cmd::WRITE_BLOCK, arg)?;
            if response != 0 {
                self.io_release()?;
                return Err(BlockError::Io);
            }
            self.write_data_block(token::START_BLOCK, chunk)?;
            self.io_release()?;
        }
        Ok(())
    }

    #[cfg(not(feature = "write"))]
    fn write_sectors(&mut self, _lba: u64, _buf: &[u8]) -> BlockResult<()> {
        Err(BlockError::WriteProtected)
    }

    fn flush(&mut self) -> BlockResult<()> {
        self.bus.set_chip_select(true);
        let result = self.wait_not_busy();
        self.io_release()?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimCard;

    #[test]
    fn identifies_an_sdhc_card() {
        let card = SdCard::init(SimCard::new(CardType::Sdhc)).unwrap();
        assert_eq!(card.card_type(), CardType::Sdhc);
        assert!(card.card_type().block_addressed());
        assert!(!card.card_type().is_mmc());
    }

    /// THE ACCEPT COLUMN: a card that offers High Speed is switched, and the clock request follows.
    ///
    /// Both halves are asserted because they are separate claims and only one of them is the
    /// point. The card's own `access_mode` says the switch STUCK -- the sim reports it from state a
    /// mode-1 command moved, not from the reply it just sent -- and the requested rate says the
    /// host acted on it. A driver that switched and left the bus at 25 MHz would pass the first and
    /// deliver nothing.
    #[test]
    fn high_speed_is_switched_and_the_bus_rate_follows_it() {
        let mut card = SdCard::init(SimCard::new(CardType::Sdhc)).unwrap();
        assert!(card.try_high_speed().unwrap(), "a card offering High Speed must be switched");

        let bus = card.release_bus();
        assert_eq!(bus.access_mode(), 1, "the CARD is in High Speed, not just the host's belief");
        assert_eq!(bus.requested_clock_hz(), switch::HIGH_SPEED_CLOCK_HZ);
    }

    /// THE REJECT COLUMN, AND IT IS THE ONE THAT MATTERS: a card that implements `CMD6` and does
    /// NOT offer High Speed must be left at its default rate.
    ///
    /// **This is the case a wrong driver passes silently.** Mode 1 against an unsupported function
    /// is legal and the card answers OK; only the result nibble says it declined. A driver that
    /// switched without querying, or queried without reading the answer, would raise the bus to
    /// 50 MHz against a card whose ceiling is 25 -- and the failure would appear later as corrupt
    /// data on a link everyone believed was in spec.
    #[test]
    fn a_card_that_declines_high_speed_is_left_at_its_default_rate() {
        let mut card = SdCard::init(SimCard::without_high_speed(CardType::Sdhc)).unwrap();

        assert!(!card.try_high_speed().unwrap(), "an unsupported function must not report success");

        let bus = card.release_bus();
        assert_eq!(bus.access_mode(), 0, "the card must still be in default speed");
        assert_eq!(
            bus.requested_clock_hz(),
            25_000_000,
            "the bus must NOT have been asked for a rate the card does not permit"
        );
    }

    /// A card with no `CMD6` at all -- an MMC here, and equally any SD card whose CSD omits command
    /// class 10 -- is not an error. It is a card without the feature.
    #[test]
    fn a_card_that_refuses_the_command_outright_is_not_a_failure() {
        let mut card = SdCard::init(SimCard::new(CardType::Mmc)).unwrap();
        assert_eq!(card.try_high_speed(), Ok(false));
        assert_eq!(card.release_bus().requested_clock_hz(), 25_000_000);
    }

    /// The query changes nothing. Asking is the safe operation, which is what makes "query before
    /// switch" a rule with no cost.
    #[test]
    fn querying_the_function_does_not_select_it() {
        let mut card = SdCard::init(SimCard::new(CardType::Sdhc)).unwrap();
        let status = card
            .query_function(switch::GROUP_ACCESS_MODE, switch::FUNCTION_HIGH_SPEED)
            .unwrap()
            .expect("an SD card implements CMD6");

        assert!(switch::supports(&status, switch::GROUP_ACCESS_MODE, switch::FUNCTION_HIGH_SPEED));
        assert_eq!(
            card.release_bus().access_mode(),
            0,
            "mode 0 reports what the card WOULD grant and must not grant it"
        );
    }

    #[test]
    fn identifies_a_standard_capacity_v2_card() {
        let card = SdCard::init(SimCard::new(CardType::Sd2)).unwrap();
        assert_eq!(card.card_type(), CardType::Sd2);
        assert!(!card.card_type().block_addressed());
    }

    #[test]
    fn identifies_a_version_1_card() {
        let card = SdCard::init(SimCard::new(CardType::Sd1)).unwrap();
        assert_eq!(card.card_type(), CardType::Sd1);
        assert!(!card.card_type().block_addressed());
        assert!(!card.card_type().is_mmc());
    }

    #[test]
    fn identifies_an_mmc_via_the_cmd1_branch() {
        let card = SdCard::init(SimCard::new(CardType::Mmc)).unwrap();
        assert_eq!(card.card_type(), CardType::Mmc);
        assert!(card.card_type().is_mmc());
        assert!(!card.card_type().block_addressed());
    }

    #[test]
    fn an_empty_slot_is_reported_as_no_card() {
        let result = SdCard::init(SimCard::absent());
        assert_eq!(result.unwrap_err(), InitError::NoCard);
    }

    #[test]
    fn a_wrong_cmd8_echo_is_rejected() {
        let result = SdCard::init(SimCard::with_bad_echo(CardType::Sd2));
        assert_eq!(result.unwrap_err(), InitError::BadInterfaceCondition);
    }

    #[test]
    fn reports_the_sector_count_from_the_csd() {
        let mut sdhc = SdCard::init(SimCard::new(CardType::Sdhc)).unwrap();
        assert_eq!(sdhc.sector_count().unwrap(), 8192 * 1024);
        let mut sdsc = SdCard::init(SimCard::new(CardType::Sd2)).unwrap();
        assert_eq!(sdsc.sector_count().unwrap(), 3752 * 128);
    }

    #[cfg(feature = "write")]
    #[test]
    fn a_written_sector_reads_back_and_leaves_its_neighbour_alone() {
        let mut card = SdCard::init(SimCard::new(CardType::Sdhc)).unwrap();
        let mut written = [0u8; SECTOR_SIZE];
        for (i, byte) in written.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        card.write_sectors(5, &written).unwrap();

        let mut read = [0xAAu8; SECTOR_SIZE];
        card.read_sectors(5, &mut read).unwrap();
        assert_eq!(read, written);

        let mut neighbour = [0xAAu8; SECTOR_SIZE];
        card.read_sectors(4, &mut neighbour).unwrap();
        assert_eq!(neighbour, [0u8; SECTOR_SIZE]);
    }

    #[cfg(feature = "write")]
    #[test]
    fn a_multi_sector_range_round_trips_on_a_byte_addressed_card() {
        let mut card = SdCard::init(SimCard::new(CardType::Sd1)).unwrap();
        let mut written = [0u8; SECTOR_SIZE * 3];
        for (i, byte) in written.iter_mut().enumerate() {
            *byte = (i / SECTOR_SIZE) as u8;
        }
        card.write_sectors(2, &written).unwrap();

        let mut read = [0xFFu8; SECTOR_SIZE * 3];
        card.read_sectors(2, &mut read).unwrap();
        assert_eq!(read, written);
    }

    #[test]
    fn a_read_past_the_capacity_is_out_of_range() {
        let mut card = SdCard::init(SimCard::new(CardType::Sd2)).unwrap();
        let count = card.sector_count().unwrap();
        let mut buf = [0u8; SECTOR_SIZE];
        assert_eq!(card.read_sectors(count, &mut buf).unwrap_err(), BlockError::OutOfRange);
    }

    #[test]
    fn flush_succeeds_on_an_idle_card() {
        let mut card = SdCard::init(SimCard::new(CardType::Sdhc)).unwrap();
        assert!(card.flush().is_ok());
    }

    #[test]
    fn a_multi_block_read_streams_a_range_back_to_back() {
        let mut written = [0u8; SECTOR_SIZE * 8];
        for (i, byte) in written.iter_mut().enumerate() {
            *byte = ((i / SECTOR_SIZE) * 17 + 1) as u8;
        }
        let mut sim = SimCard::new(CardType::Sdhc);
        sim.seed(0, &written);
        let mut card = SdCard::init(sim).unwrap();
        let mut read = [0u8; SECTOR_SIZE * 8];
        card.read_sectors(0, &mut read).unwrap();
        assert_eq!(read, written);

        let mut one = [0u8; SECTOR_SIZE];
        card.read_sectors(3, &mut one).unwrap();
        assert_eq!(one, written[3 * SECTOR_SIZE..4 * SECTOR_SIZE]);
    }

    #[cfg(not(feature = "write"))]
    #[test]
    fn a_write_off_build_refuses_writes_as_write_protected() {
        let mut card = SdCard::init(SimCard::new(CardType::Sdhc)).unwrap();
        let buf = [0u8; SECTOR_SIZE];
        assert_eq!(card.write_sectors(0, &buf).unwrap_err(), BlockError::WriteProtected);
        let mut read = [0u8; SECTOR_SIZE];
        assert!(card.read_sectors(0, &mut read).is_ok());
    }
}
