//! The WINC SPI slave protocol (layer 1): register reads/writes carried over a board's
//! [`SpiBus`](crate::SpiBus) -- the foundation every HIF exchange rides on.

use crate::SpiBus;

/// Command bytes: start nibble 4'b1100 | type (SDG 17.1.1).
pub const CMD_DMA_WRITE: u8 = 0xc1;
pub const CMD_DMA_READ: u8 = 0xc2;
pub const CMD_INTERNAL_WRITE: u8 = 0xc3;
pub const CMD_INTERNAL_READ: u8 = 0xc4;
pub const CMD_TERMINATE: u8 = 0xc5;
pub const CMD_REPEAT: u8 = 0xc6;
pub const CMD_DMA_EXT_WRITE: u8 = 0xc7;
pub const CMD_DMA_EXT_READ: u8 = 0xc8;
pub const CMD_SINGLE_WRITE: u8 = 0xc9;
pub const CMD_SINGLE_READ: u8 = 0xca;
pub const CMD_RESET: u8 = 0xcf;

/// The chip-identity register (vendor driver `nmasic.h`: `NMI_CHIPID` at the peripheral base).
pub const NMI_CHIPID: u32 = 0x1000;
/// The WINC SPI block's protocol-config register (`NMI_SPI_REG_BASE 0xe800` + 0x24): CRC checking
/// enable bits [3:2] and the data-packet-size field [6:4].
pub const NMI_SPI_PROTOCOL_CONFIG: u32 = 0xe824;

/// Registers at or below this offset are the module's CLOCKLESS registers: readable via the
/// internal-read command with the clockless marker even before the chip's main clock runs
/// (reference driver read path).
const CLOCKLESS_READ_LIMIT: u32 = 0xff;
/// The write path's clockless bound in the reference driver.
const CLOCKLESS_WRITE_LIMIT: u32 = 0x30;

/// Single-byte polls allowed for a response byte before declaring the exchange dead
/// (`SPI_RESP_RETRY_COUNT` in the reference).
const RESPONSE_RETRIES: usize = 10;

/// A protocol-level failure. `Bus` wraps the board transport's own error; the rest mean the
/// module answered wrongly (or not at all), which after a hard reset points at wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiError {
    /// The underlying SPI transport failed.
    Bus,
    /// No command echo arrived within the retry budget.
    NoEcho { cmd: u8, last: u8 },
    /// The state byte after the echo was not the no-error value 0x00.
    BadState(u8),
    /// No data-packet header (start nibble 4'b1111) arrived within the retry budget.
    NoDataHeader { last: u8 },
}

/// CRC7 over `bytes` with polynomial G(x) = x^7 + x^3 + 1 and seed 0x7F (SDG 17.1.1), bitwise --
/// equivalent to the reference driver's syndrome table.
fn crc7(bytes: &[u8]) -> u8 {
    let mut crc: u8 = 0x7f;
    for &byte in bytes {
        let mut data = byte;
        for _ in 0..8 {
            let msb = (crc >> 6) ^ (data >> 7);
            crc = ((crc << 1) ^ (msb * 0x09)) & 0x7f;
            data <<= 1;
        }
    }
    crc
}

/// The protocol driver over a board's bus: tracks whether command/data CRC framing is active
/// (on out of module reset; disabled by [`init`](Self::init)).
#[derive(Debug)]
pub struct Link<S> {
    bus: S,
    crc_enabled: bool,
}

impl<S: SpiBus> Link<S> {
    pub fn new(bus: S) -> Self {
        Self { bus, crc_enabled: true }
    }

    pub fn into_bus(self) -> S {
        self.bus
    }

    fn exchange_byte(&mut self, tx: u8) -> Result<u8, SpiError> {
        let mut rx = [0u8; 1];
        self.bus.transfer(&[tx], &mut rx).map_err(|_| SpiError::Bus)?;
        Ok(rx[0])
    }

    /// Sends a 4-byte command frame, appending the CRC7 byte while CRC framing is active. The
    /// reference driver transmits `crc7 << 1` (integrity bit 0 clear).
    fn send_command(&mut self, frame: [u8; 4]) -> Result<(), SpiError> {
        if self.crc_enabled {
            let mut with_crc = [0u8; 5];
            with_crc[..4].copy_from_slice(&frame);
            with_crc[4] = crc7(&frame) << 1;
            let mut rx = [0u8; 5];
            self.bus.transfer(&with_crc, &mut rx).map_err(|_| SpiError::Bus)
        } else {
            let mut rx = [0u8; 4];
            self.bus.transfer(&frame, &mut rx).map_err(|_| SpiError::Bus)
        }
    }

    /// Awaits the command's response pair: single-byte polls until the command echo appears,
    /// then until the state byte reads 0x00 (no error).
    fn await_response(&mut self, cmd: u8) -> Result<(), SpiError> {
        let mut last = 0;
        let mut echoed = false;
        for _ in 0..RESPONSE_RETRIES {
            last = self.exchange_byte(0)?;
            if last == cmd {
                echoed = true;
                break;
            }
        }
        if !echoed {
            return Err(SpiError::NoEcho { cmd, last });
        }
        for _ in 0..RESPONSE_RETRIES {
            let state = self.exchange_byte(0)?;
            if state == 0x00 {
                return Ok(());
            }
            last = state;
        }
        Err(SpiError::BadState(last))
    }

    /// Awaits a data packet's header (start nibble 4'b1111; low nibble = packet order) and reads
    /// `word` -- 4 register bytes, least-significant first (reference read path) -- plus the two
    /// CRC16 bytes while CRC framing is active (checked by the module direction only; ignored
    /// here, as in the reference).
    fn read_data_word(&mut self) -> Result<u32, SpiError> {
        let mut last = 0;
        let mut found = false;
        for _ in 0..RESPONSE_RETRIES {
            last = self.exchange_byte(0)?;
            if (last >> 4) == 0xf {
                found = true;
                break;
            }
        }
        if !found {
            return Err(SpiError::NoDataHeader { last });
        }
        let mut word = [0u8; 4];
        let mut rx = [0u8; 4];
        self.bus.transfer(&[0; 4], &mut rx).map_err(|_| SpiError::Bus)?;
        word.copy_from_slice(&rx);
        if self.crc_enabled {
            let mut crc_rx = [0u8; 2];
            self.bus.transfer(&[0; 2], &mut crc_rx).map_err(|_| SpiError::Bus)?;
        }
        Ok(u32::from_le_bytes(word))
    }

    /// Reads a 32-bit module register: clockless internal-read for offsets at or below 0xFF
    /// (offset high byte carries the clockless marker, bit 7), the 3-byte-address single-word
    /// read otherwise.
    pub fn read_reg(&mut self, addr: u32) -> Result<u32, SpiError> {
        let (cmd, frame) = if addr <= CLOCKLESS_READ_LIMIT {
            (
                CMD_INTERNAL_READ,
                [CMD_INTERNAL_READ, (addr >> 8) as u8 | 0x80, addr as u8, 0x00],
            )
        } else {
            (
                CMD_SINGLE_READ,
                [CMD_SINGLE_READ, (addr >> 16) as u8, (addr >> 8) as u8, addr as u8],
            )
        };
        self.send_command(frame)?;
        self.await_response(cmd)?;
        self.read_data_word()
    }

    /// Writes a 32-bit module register: clockless internal-write for offsets at or below 0x30,
    /// the single-word write otherwise. Both are ONE frame carrying address + data (SDG type C /
    /// type D), answered by echo + state.
    pub fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), SpiError> {
        let value_bytes = value.to_be_bytes();
        if addr <= CLOCKLESS_WRITE_LIMIT {
            let frame = [
                CMD_INTERNAL_WRITE,
                (addr >> 8) as u8 | 0x80,
                addr as u8,
                value_bytes[0],
                value_bytes[1],
                value_bytes[2],
                value_bytes[3],
            ];
            self.send_frame_with_crc(&frame)?;
            self.await_response(CMD_INTERNAL_WRITE)
        } else {
            let frame = [
                CMD_SINGLE_WRITE,
                (addr >> 16) as u8,
                (addr >> 8) as u8,
                addr as u8,
                value_bytes[0],
                value_bytes[1],
                value_bytes[2],
                value_bytes[3],
            ];
            self.send_frame_with_crc(&frame)?;
            self.await_response(CMD_SINGLE_WRITE)
        }
    }

    fn send_frame_with_crc(&mut self, frame: &[u8]) -> Result<(), SpiError> {
        if self.crc_enabled {
            let mut with_crc = [0u8; 9];
            with_crc[..frame.len()].copy_from_slice(frame);
            with_crc[frame.len()] = crc7(frame) << 1;
            let mut rx = [0u8; 9];
            let len = frame.len() + 1;
            self.bus.transfer(&with_crc[..len], &mut rx[..len]).map_err(|_| SpiError::Bus)
        } else {
            let mut rx = [0u8; 8];
            self.bus.transfer(frame, &mut rx[..frame.len()]).map_err(|_| SpiError::Bus)
        }
    }

    /// The soft-reset command (payload 0xFF 0xFF 0xFF).
    pub fn soft_reset(&mut self) -> Result<(), SpiError> {
        self.send_command([CMD_RESET, 0xff, 0xff, 0xff])?;
        self.await_response(CMD_RESET)
    }

    /// Largest data packet exchanged per DATA header. [`init`](Self::init) configures the
    /// module's 8K size; 1024 per packet keeps host buffers small (any multiple works -- the
    /// packet stream carries its own first/middle/last order markers).
    const DATA_PACKET: usize = 1024;

    /// Writes `data` to module memory at `addr`: the DMA extended write command, then the data
    /// as order-marked packets (header 0xF1 first / 0xF2 middle / 0xF3 last-or-only), each
    /// acknowledged -- the reference's data response is the 0xC3 marker byte followed by the
    /// no-error state.
    pub fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), SpiError> {
        let frame = [
            CMD_DMA_EXT_WRITE,
            (addr >> 16) as u8,
            (addr >> 8) as u8,
            addr as u8,
            (data.len() >> 16) as u8,
            (data.len() >> 8) as u8,
            data.len() as u8,
        ];
        self.send_frame_with_crc(&frame)?;
        self.await_response(CMD_DMA_EXT_WRITE)?;
        let mut remaining = data;
        let mut first = true;
        while !remaining.is_empty() {
            let take = remaining.len().min(Self::DATA_PACKET);
            let last = take == remaining.len();
            let order: u8 = if last { 0x3 } else if first { 0x1 } else { 0x2 };
            self.exchange_byte(0xf0 | order)?;
            let mut rx = [0u8; 64];
            for chunk in remaining[..take].chunks(rx.len()) {
                self.bus.transfer(chunk, &mut rx[..chunk.len()]).map_err(|_| SpiError::Bus)?;
            }
            remaining = &remaining[take..];
            first = false;
        }
        let mut last_seen = 0;
        for _ in 0..RESPONSE_RETRIES {
            last_seen = self.exchange_byte(0)?;
            if last_seen == 0xc3 {
                let state = self.exchange_byte(0)?;
                if state == 0x00 {
                    return Ok(());
                }
                return Err(SpiError::BadState(state));
            }
        }
        Err(SpiError::NoEcho { cmd: 0xc3, last: last_seen })
    }

    /// Reads `buf.len()` bytes of module memory at `addr`: the DMA extended read command, its
    /// echo + state, then order-marked data packets (each preceded by its 0xF? header).
    pub fn read_block(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), SpiError> {
        let frame = [
            CMD_DMA_EXT_READ,
            (addr >> 16) as u8,
            (addr >> 8) as u8,
            addr as u8,
            (buf.len() >> 16) as u8,
            (buf.len() >> 8) as u8,
            buf.len() as u8,
        ];
        self.send_frame_with_crc(&frame)?;
        self.await_response(CMD_DMA_EXT_READ)?;
        let total = buf.len();
        let mut done = 0;
        while done < total {
            let take = (total - done).min(Self::DATA_PACKET);
            let mut last = 0;
            let mut found = false;
            for _ in 0..RESPONSE_RETRIES {
                last = self.exchange_byte(0)?;
                if (last >> 4) == 0xf {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(SpiError::NoDataHeader { last });
            }
            let mut tx = [0u8; 64];
            let mut taken = 0;
            while taken < take {
                let n = (take - taken).min(tx.len());
                self.bus
                    .transfer(&tx[..n], &mut buf[done + taken..done + taken + n])
                    .map_err(|_| SpiError::Bus)?;
                taken += n;
            }
            if self.crc_enabled {
                let mut crc_rx = [0u8; 2];
                self.bus.transfer(&[0; 2], &mut crc_rx).map_err(|_| SpiError::Bus)?;
            }
            done += take;
            let _ = tx;
        }
        Ok(())
    }

    /// The reference boot sequence: read the protocol-config register (retrying with CRC-off
    /// framing if the module kept CRC-less state from an earlier session), then disable CRC
    /// checking and select the 8K data-packet size -- after which every frame is CRC-less.
    /// Returns the module's protocol-config value.
    pub fn init(&mut self) -> Result<u32, SpiError> {
        self.crc_enabled = true;
        let config = match self.read_reg(NMI_SPI_PROTOCOL_CONFIG) {
            Ok(config) => config,
            Err(_) => {
                self.crc_enabled = false;
                self.read_reg(NMI_SPI_PROTOCOL_CONFIG)?
            }
        };
        if self.crc_enabled {
            let reconfigured = (config & !0x0c & !0x70) | (0x5 << 4);
            self.write_reg(NMI_SPI_PROTOCOL_CONFIG, reconfigured)?;
            self.crc_enabled = false;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;

    /// A scripted WINC slave: records every transmitted byte and answers from a queue.
    struct MockBus {
        sent: Vec<u8>,
        replies: VecDeque<u8>,
    }

    impl MockBus {
        fn replying(bytes: &[u8]) -> Self {
            Self { sent: Vec::new(), replies: bytes.iter().copied().collect() }
        }
    }

    impl SpiBus for MockBus {
        type Error = ();
        fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), ()> {
            self.sent.extend_from_slice(tx);
            for slot in rx.iter_mut() {
                *slot = self.replies.pop_front().unwrap_or(0);
            }
            Ok(())
        }
    }

    #[test]
    fn crc7_is_stable_and_payload_sensitive() {
        let a = crc7(&[CMD_INTERNAL_READ, 0x80, 0x24, 0x00]);
        let b = crc7(&[CMD_INTERNAL_READ, 0x80, 0x25, 0x00]);
        assert!(a <= 0x7f && b <= 0x7f);
        assert_ne!(a, b);
        assert_eq!(a, crc7(&[CMD_INTERNAL_READ, 0x80, 0x24, 0x00]));
    }


    #[test]
    fn chip_id_read_uses_single_read_with_24_bit_address() {
        let mut link = Link::new(MockBus::replying(&[
            0, 0, 0, 0, 0,
            CMD_SINGLE_READ,
            0x00,
            0xf3,
            0xa0,
            0x03,
            0x15,
            0x00,
            0xaa,
            0xbb,
        ]));
        let id = link.read_reg(NMI_CHIPID).expect("chip id");
        assert_eq!(id, 0x001503a0);
        assert_eq!(&link.bus.sent[..4], &[CMD_SINGLE_READ, 0x00, 0x10, 0x00]);
        assert_eq!(link.bus.sent[4], crc7(&[CMD_SINGLE_READ, 0x00, 0x10, 0x00]) << 1);
    }

    #[test]
    fn low_offsets_read_clockless_via_internal_read() {
        let mut link = Link::new(MockBus::replying(&[
            0, 0, 0, 0, 0,
            CMD_INTERNAL_READ,
            0x00,
            0xf3,
            0x01,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ]));
        let value = link.read_reg(0x24).expect("clockless read");
        assert_eq!(value, 1);
        assert_eq!(&link.bus.sent[..4], &[CMD_INTERNAL_READ, 0x80, 0x24, 0x00]);
    }

    #[test]
    fn echo_can_arrive_after_leading_idle_bytes() {
        let mut link = Link::new(MockBus::replying(&[
            0, 0, 0, 0, 0,
            0x00,
            0x00,
            CMD_SINGLE_READ,
            0x00,
            0x00,
            0xf3,
            0x2a,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ]));
        assert_eq!(link.read_reg(0x1000).expect("read"), 42);
    }

    #[test]
    fn missing_echo_reports_no_echo() {
        let mut link = Link::new(MockBus::replying(&[0u8; 32]));
        assert_eq!(
            link.read_reg(0x1000),
            Err(SpiError::NoEcho { cmd: CMD_SINGLE_READ, last: 0 })
        );
    }

    #[test]
    fn init_disables_crc_framing() {
        let mut link = Link::new(MockBus::replying(&[
            0, 0, 0, 0, 0,
            CMD_SINGLE_READ,
            0x00,
            0xf3,
            0x01,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0, 0, 0, 0, 0, 0, 0, 0, 0,
            CMD_SINGLE_WRITE,
            0x00,
        ]));
        let config = link.init().expect("init");
        assert_eq!(config, 1);
        assert!(!link.crc_enabled);
        let crc_byte = 1;
        let write_frame_start = link.bus.sent.len() - 2 - 8 - crc_byte;
        assert_eq!(link.bus.sent[write_frame_start], CMD_SINGLE_WRITE);
        assert_eq!(link.bus.sent[write_frame_start + 7], 0x51);
    }
}
