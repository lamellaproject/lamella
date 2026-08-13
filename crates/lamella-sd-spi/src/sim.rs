//! An in-memory memory card that answers the [`SdSpiBus`] seam at the command level, for host
//! tests with no hardware.

use crate::card::CardType;
use crate::{data_response, r1, token, SdSpiBus, SEND_IF_COND_CHECK_PATTERN};
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::convert::Infallible;
use lamella_cil_runtime::block::SECTOR_SIZE;

/// Op-cond polls a card answers "still idle" before it reports ready -- enough to exercise the
/// driver's polling loop without dragging the tests out.
const OP_READY_AFTER: u32 = 3;

/// The default medium size for [`SimCard::new`]: small, because the protocol tests touch only low
/// sectors while the reported CSD capacity is large, so the driver's range check passes and the sim
/// serves from this store. A file-system test wants a store sized to a real volume and matched to
/// the reported capacity -- see [`SimCard::with_capacity`].
const STORE_SECTORS: usize = 64;

/// A memory card reached over an [`SdSpiBus`], simulated in memory.
#[derive(Debug)]
pub struct SimCard {
    kind: CardType,
    /// `false` models an empty slot: the data-out line floats high, so every read is `0xFF`.
    present: bool,
    /// Makes a v2 card echo the wrong `CMD8` check pattern -- the bad-card control.
    bad_echo: bool,
    cs_asserted: bool,
    /// The partial command frame accumulated so far (reset when chip-select drops).
    frame: Vec<u8>,
    /// Bytes queued to clock back on subsequent reads.
    outbox: VecDeque<u8>,
    /// Set by `CMD55`; the next command is application-specific. Survives a chip-select drop
    /// (a real card holds the APP state across the 8 release clocks), cleared by any command.
    app_pending: bool,
    /// How many op-cond polls have happened; the card readies after a few, exercising the loop.
    op_polls: u32,
    ready: bool,
    /// The medium: a whole number of sectors of backing bytes.
    store: Vec<u8>,
    /// The 16-byte CSD this card reports to `CMD9`.
    csd: [u8; 16],
    /// Whether this card OFFERS High Speed in `CMD6`'s group 1. A card can implement the command
    /// and decline the function, which is the case that separates "asked and got it" from "asked
    /// and was told no" -- and the one a test that only ever models a capable card cannot reach.
    high_speed_offered: bool,
    /// The group-1 function currently selected: 0 default speed, 1 High Speed. A successful mode-1
    /// switch moves it, and every later status reports it, so a test can prove the switch STUCK
    /// rather than merely that the card answered.
    access_mode: u8,
    /// The last rate the host ASKED the bus for. A sim has no divisor to clamp with, so this is the
    /// request verbatim.
    requested_clock_hz: u32,
    /// The sector a `CMD24` write is targeting, once its data block arrives.
    write_target: Option<u64>,
    /// Whether the write's start-block token has been seen (data bytes follow).
    write_started: bool,
    /// The write data block accumulated so far (512 data + 2 CRC).
    write_buf: Vec<u8>,
    /// The next sector a `CMD18` multi-block read will stream; `None` when not streaming. A real
    /// card streams blocks back-to-back until `CMD12`, so the sim refills the outbox with the next
    /// block whenever it empties mid-stream.
    streaming_read: Option<u64>,
}

impl SimCard {
    /// A present, healthy card of `kind` over a small default store, reporting a large plausible
    /// capacity. Right for the protocol tests, which touch only low sectors; for a file-system test
    /// use [`with_capacity`](Self::with_capacity), which matches the store to the reported size.
    #[must_use]
    pub fn new(kind: CardType) -> Self {
        SimCard {
            kind,
            present: true,
            bad_echo: false,
            cs_asserted: false,
            frame: Vec::new(),
            outbox: VecDeque::new(),
            app_pending: false,
            op_polls: 0,
            ready: false,
            store: alloc::vec![0u8; STORE_SECTORS * SECTOR_SIZE],
            csd: default_csd(kind),
            high_speed_offered: true,
            access_mode: 0,
            requested_clock_hz: 0,
            write_target: None,
            write_started: false,
            write_buf: Vec::new(),
            streaming_read: None,
        }
    }

    /// A present card of `kind` whose store is exactly `store_sectors` sectors AND whose reported
    /// CSD capacity equals that -- so a formatter sizes its volume to the medium and every sector a
    /// file system reaches is backed. `store_sectors` must be a whole number of 512 KB units (a
    /// multiple of 1024 sectors), which both the v2 (block-addressed) and v1 (byte-addressed) CSD
    /// encodings represent exactly.
    #[must_use]
    pub fn with_capacity(kind: CardType, store_sectors: u64) -> Self {
        let mut card = SimCard::new(kind);
        card.store = alloc::vec![0u8; store_sectors as usize * SECTOR_SIZE];
        card.csd = capacity_csd(kind, store_sectors);
        card
    }

    /// A card that implements `CMD6` and does NOT offer High Speed -- the reject column for the
    /// bus-speed switch.
    ///
    /// It is a distinct card from an MMC, which refuses the command outright: this one answers
    /// successfully, reports the function as unsupported, and leaves a mode-1 switch reporting
    /// "no influence". **That is the shape a host can mistake for success**, so it is the one worth
    /// having a card for.
    #[must_use]
    pub fn without_high_speed(kind: CardType) -> Self {
        let mut card = SimCard::new(kind);
        card.high_speed_offered = false;
        card
    }

    /// The last bus rate the host asked for, in hertz.
    #[must_use]
    pub fn requested_clock_hz(&self) -> u32 {
        self.requested_clock_hz
    }

    /// The group-1 function this card has selected: 0 default speed, 1 High Speed.
    ///
    /// The card's own state, not the host's belief about it -- which is what makes it worth
    /// asserting separately from the clock the host went on to request.
    #[must_use]
    pub fn access_mode(&self) -> u8 {
        self.access_mode
    }

    /// An empty slot: every read floats high (`0xFF`), so `CMD0` never sees an idle response.
    #[must_use]
    pub fn absent() -> Self {
        let mut card = SimCard::new(CardType::Sd2);
        card.present = false;
        card
    }

    /// A v2 card that echoes the wrong `CMD8` check pattern -- the "did not understand the command"
    /// control.
    #[must_use]
    pub fn with_bad_echo(kind: CardType) -> Self {
        let mut card = SimCard::new(kind);
        card.bad_echo = true;
        card
    }

    /// Writes bytes straight into the store, bypassing the driver's write path, so a READ test can
    /// seed a card without depending on the `write` feature.
    pub fn seed(&mut self, sector: u64, data: &[u8]) {
        let start = sector as usize * SECTOR_SIZE;
        self.store[start..start + data.len()].copy_from_slice(data);
    }

    /// Queues one read data block (start token + 512 sector bytes + 2 CRC) for `sector`, or a
    /// data-error token if it is beyond the store (a mis-addressed read).
    fn queue_read_block(&mut self, sector: u64) {
        let start = sector as usize * SECTOR_SIZE;
        if start + SECTOR_SIZE <= self.store.len() {
            self.outbox.push_back(token::START_BLOCK);
            for offset in 0..SECTOR_SIZE {
                self.outbox.push_back(self.store[start + offset]);
            }
            self.outbox.push_back(0xFF);
            self.outbox.push_back(0xFF);
        } else {
            self.outbox.push_back(0x08);
        }
    }

    /// The sector index a read/write command's argument names, decoded the way a real card of this
    /// family would: a byte offset on a byte-addressed card, the sector itself on SDHC.
    fn decode_sector(&self, arg: u32) -> u64 {
        if matches!(self.kind, CardType::Sdhc) {
            u64::from(arg)
        } else {
            u64::from(arg) / SECTOR_SIZE as u64
        }
    }

    /// The R1 idle bit reflects whether the card has finished initializing.
    fn idle_r1(&self) -> u8 {
        if self.ready {
            0x00
        } else {
            r1::IDLE_STATE
        }
    }

    /// Queue a response: one command-to-response latency byte, then the bytes themselves.
    fn reply(&mut self, bytes: &[u8]) {
        self.outbox.push_back(0xFF);
        for &byte in bytes {
            self.outbox.push_back(byte);
        }
    }

    fn feed(&mut self, byte: u8) {
        if self.write_target.is_some() {
            self.feed_write(byte);
            return;
        }
        if self.frame.is_empty() {
            if byte & 0xC0 == 0x40 {
                self.frame.push(byte);
            }
        } else {
            self.frame.push(byte);
            if self.frame.len() == 6 {
                let frame = core::mem::take(&mut self.frame);
                self.process(&frame);
            }
        }
    }

    /// Accumulates a write data block: skip until the start-block token, then 512 data + 2 CRC,
    /// then commit to the store and queue the data-response.
    fn feed_write(&mut self, byte: u8) {
        if !self.write_started {
            if byte == token::START_BLOCK {
                self.write_started = true;
            }
            return;
        }
        self.write_buf.push(byte);
        if self.write_buf.len() == SECTOR_SIZE + 2 {
            let sector = self.write_target.take().unwrap() as usize;
            self.write_started = false;
            let start = sector * SECTOR_SIZE;
            let response = if start + SECTOR_SIZE <= self.store.len() {
                self.store[start..start + SECTOR_SIZE].copy_from_slice(&self.write_buf[..SECTOR_SIZE]);
                data_response::ACCEPTED
            } else {
                data_response::REJECTED_WRITE_ERROR
            };
            self.write_buf.clear();
            self.outbox.push_back(0xFF);
            self.outbox.push_back(response);
            self.outbox.push_back(0x00);
        }
    }

    fn process(&mut self, frame: &[u8]) {
        let index = frame[0] & 0x3F;
        let arg = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
        let was_app = self.app_pending;
        self.app_pending = false;
        let is_sd_family = !matches!(self.kind, CardType::Mmc);
        match index {
            0 => self.reply(&[r1::IDLE_STATE]),
            8 => {
                if matches!(self.kind, CardType::Sd2 | CardType::Sdhc) {
                    let echo = if self.bad_echo { 0x00 } else { SEND_IF_COND_CHECK_PATTERN };
                    self.reply(&[r1::IDLE_STATE, 0x00, 0x00, 0x01, echo]);
                } else {
                    self.reply(&[r1::IDLE_STATE | r1::ILLEGAL_COMMAND]);
                }
            }
            55 => {
                self.app_pending = true;
                let bit = self.idle_r1();
                self.reply(&[bit]);
            }
            41 if was_app => {
                if is_sd_family {
                    self.op_polls += 1;
                    if self.op_polls >= OP_READY_AFTER {
                        self.ready = true;
                    }
                    let bit = self.idle_r1();
                    self.reply(&[bit]);
                } else {
                    self.reply(&[r1::ILLEGAL_COMMAND]);
                }
            }
            1 => {
                if self.kind == CardType::Mmc {
                    self.op_polls += 1;
                    if self.op_polls >= OP_READY_AFTER {
                        self.ready = true;
                    }
                    let bit = self.idle_r1();
                    self.reply(&[bit]);
                } else {
                    self.reply(&[r1::ILLEGAL_COMMAND]);
                }
            }
            58 => {
                let byte0 = if self.kind == CardType::Sdhc { 0xC0 } else { 0x80 };
                self.reply(&[0x00, byte0, 0xFF, 0x80, 0x00]);
            }
            16 => self.reply(&[0x00]),
            9 => {
                self.outbox.push_back(0xFF);
                self.outbox.push_back(0x00);
                self.outbox.push_back(token::START_BLOCK);
                for &byte in &self.csd {
                    self.outbox.push_back(byte);
                }
                self.outbox.push_back(0xFF);
                self.outbox.push_back(0xFF);
            }
            6 => {
                if self.kind == CardType::Mmc {
                    self.reply(&[r1::ILLEGAL_COMMAND]);
                } else {
                    let set = arg >> 31 != 0;
                    let requested = (arg & 0xF) as u8;
                    let grantable = requested == 0 || (requested == 1 && self.high_speed_offered);
                    if set && grantable {
                        self.access_mode = requested;
                    }
                    let status = self.switch_status(requested, grantable);
                    self.outbox.push_back(0xFF);
                    self.outbox.push_back(0x00);
                    self.outbox.push_back(token::START_BLOCK);
                    for byte in status {
                        self.outbox.push_back(byte);
                    }
                    self.outbox.push_back(0xFF);
                    self.outbox.push_back(0xFF);
                }
            }
            17 => {
                let sector = self.decode_sector(arg);
                self.outbox.push_back(0xFF);
                self.outbox.push_back(0x00);
                self.queue_read_block(sector);
            }
            18 => {
                let sector = self.decode_sector(arg);
                self.outbox.push_back(0xFF);
                self.outbox.push_back(0x00);
                self.queue_read_block(sector);
                self.streaming_read = Some(sector + 1);
            }
            12 => {
                self.streaming_read = None;
                self.outbox.clear();
                self.reply(&[0x00, 0x00]);
            }
            24 => {
                self.write_target = Some(self.decode_sector(arg));
                self.reply(&[0x00]);
            }
            _ => self.reply(&[r1::ILLEGAL_COMMAND]),
        }
    }
}

impl SimCard {
    /// The 64-byte `CMD6` function status this card reports: what it offers in each group, and
    /// which function each group has selected.
    ///
    /// The layout is most-significant bit first over 512 bits, so group 1's support bitmap lands in
    /// bytes 12 and 13 and its selection result in the low nibble of byte 16. Only group 1 is
    /// modelled with any care; the rest offer their default function and nothing else, which is
    /// what a card with no optional command system or driver strength reports.
    fn switch_status(&self, requested: u8, grantable: bool) -> [u8; crate::switch::STATUS_LEN] {
        let mut status = [0u8; crate::switch::STATUS_LEN];
        status[0] = 0x00;
        status[1] = 200;
        status[12] = 0x00;
        status[13] = if self.high_speed_offered { 0b11 } else { 0b01 };
        for group in 2..=6usize {
            status[13 - 2 * (group - 1)] = 0b01;
        }
        let result = if requested == crate::switch::NO_INFLUENCE {
            self.access_mode
        } else if grantable {
            requested
        } else {
            crate::switch::NO_INFLUENCE
        };
        status[16] = result & 0xF;
        status[17] = 1;
        status
    }
}

/// A CSD reporting a plausible large capacity for `kind`: a v2 (block-addressed) layout for SDHC,
/// a v1 (exponent) layout otherwise. The exact values match the crate-root CSD unit tests, so
/// `csd_sector_count` parses them to a known count.
fn default_csd(kind: CardType) -> [u8; 16] {
    let mut csd = [0u8; 16];
    if matches!(kind, CardType::Sdhc) {
        csd[0] = 0x40;
        let c_size: u32 = 8191;
        csd[7] = ((c_size >> 16) & 0x3F) as u8;
        csd[8] = ((c_size >> 8) & 0xFF) as u8;
        csd[9] = (c_size & 0xFF) as u8;
    } else {
        csd[5] = 9;
        let c_size: u32 = 3751;
        csd[6] = ((c_size >> 10) & 0x03) as u8;
        csd[7] = ((c_size >> 2) & 0xFF) as u8;
        csd[8] = ((c_size & 0x03) << 6) as u8;
        let mult: u32 = 5;
        csd[9] = ((mult >> 1) & 0x03) as u8;
        csd[10] = ((mult & 1) << 7) as u8;
    }
    csd
}

/// A CSD reporting EXACTLY `store_sectors` sectors for `kind`, so a formatter fills the whole
/// simulated medium and no sector it reaches falls outside the store. The bit layout is the one
/// `csd_sector_count` decodes; `store_sectors` must be a multiple of 1024.
fn capacity_csd(kind: CardType, store_sectors: u64) -> [u8; 16] {
    assert!(
        store_sectors >= 1024 && store_sectors % 1024 == 0,
        "a simulated capacity must be a whole number of 512 KB units (multiple of 1024 sectors)",
    );
    let mut csd = [0u8; 16];
    if kind.block_addressed() {
        let c_size = (store_sectors / 1024 - 1) as u32;
        assert!(c_size <= 0x003F_FFFF, "capacity exceeds the CSD v2 C_SIZE field");
        csd[0] = 0x40;
        csd[7] = ((c_size >> 16) & 0x3F) as u8;
        csd[8] = ((c_size >> 8) & 0xFF) as u8;
        csd[9] = (c_size & 0xFF) as u8;
    } else {
        let c_size = (store_sectors / 512 - 1) as u32;
        assert!(c_size <= 0x0FFF, "capacity exceeds the CSD v1 C_SIZE field");
        csd[5] = 9;
        csd[6] = ((c_size >> 10) & 0x03) as u8;
        csd[7] = ((c_size >> 2) & 0xFF) as u8;
        csd[8] = ((c_size & 0x03) << 6) as u8;
        let mult: u32 = 7;
        csd[9] = ((mult >> 1) & 0x03) as u8;
        csd[10] = ((mult & 1) << 7) as u8;
    }
    csd
}

impl SdSpiBus for SimCard {
    type Error = Infallible;

    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), Self::Error> {
        assert_eq!(tx.len(), rx.len());
        for i in 0..tx.len() {
            if self.present && self.cs_asserted && self.outbox.is_empty() {
                if let Some(sector) = self.streaming_read {
                    self.queue_read_block(sector);
                    self.streaming_read = Some(sector + 1);
                }
            }
            rx[i] = if self.present { self.outbox.pop_front().unwrap_or(0xFF) } else { 0xFF };
            if self.present && self.cs_asserted {
                self.feed(tx[i]);
            }
        }
        Ok(())
    }

    fn set_chip_select(&mut self, asserted: bool) {
        if !asserted {
            self.frame.clear();
        }
        self.cs_asserted = asserted;
    }

    fn set_clock_hz(&mut self, hz: u32) {
        self.requested_clock_hz = hz;
    }

    fn delay_ms(&mut self, _ms: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csd_sector_count;

    #[test]
    fn capacity_csd_round_trips_for_both_families() {
        for &sectors in &[1024u64, 2048, 8192, 65536] {
            let block = capacity_csd(CardType::Sdhc, sectors);
            assert_eq!(csd_sector_count(&block, false), Ok(sectors), "block-addressed {sectors}");
            let byte = capacity_csd(CardType::Sd2, sectors);
            assert_eq!(csd_sector_count(&byte, false), Ok(sectors), "byte-addressed {sectors}");
        }
    }
}
