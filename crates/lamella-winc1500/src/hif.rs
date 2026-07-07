//! The WINC Host Interface (HIF) message layer: every Wi-Fi control operation and socket
//! exchange is a HIF message -- an 8-byte header (group id, opcode, 16-bit big-endian length
//! covering the header) followed by request-specific payload -- carried through module memory
//! the firmware allocates per message.

use crate::SpiBus;
use crate::boot::Registers;
use crate::spi::{Link, SpiError};

/// HIF request groups (vendor `tenuM2mReqGroup`).
pub const GROUP_MAIN: u8 = 0;
pub const GROUP_WIFI: u8 = 1;
pub const GROUP_IP: u8 = 2;
pub const GROUP_HIF: u8 = 3;
pub const GROUP_OTA: u8 = 4;
pub const GROUP_SSL: u8 = 5;

/// The data-message marker: an opcode's bit 7 flags a data (not configuration) request.
pub const OPCODE_DATA_BIT: u8 = 0x80;

const WIFI_HOST_RCV_CTRL_0: u32 = 0x1070;
const WIFI_HOST_RCV_CTRL_1: u32 = 0x1084;
const WIFI_HOST_RCV_CTRL_2: u32 = 0x1078;
const WIFI_HOST_RCV_CTRL_3: u32 = 0x106c;
const WIFI_HOST_RCV_CTRL_4: u32 = 0x15_0400;
const NMI_STATE: u32 = 0x108c;

/// Header bytes preceding every HIF payload (the vendor's `M2M_HIF_HDR_OFFSET`: the 4-byte
/// header padded to 8).
pub const HEADER_LEN: u16 = 8;
/// Largest whole HIF message the firmware accepts (`M2M_HIF_MAX_PACKET_SIZE`).
pub const MAX_MESSAGE: u16 = 1600 - 4;

/// How many times the send handshake polls for the firmware's buffer allocation before giving
/// up. The budget is TRANSFER-paced, not wall-clock: each iteration is one register read, so on
/// a hardware SERCOM at 12 MHz (~10 us/read) the reference's 1000-iteration bound gives the
/// firmware only ~10 ms to allocate -- shorter than its worst-case latency under load -- while a
/// bit-banged transport stretches the same count into minutes. 20_000 keeps a fast transport's
/// budget at ~200 ms; the loop exits on the first success either way.
const ALLOC_RETRIES: u32 = 20_000;

/// Module-memory access -- registers plus DMA block transfers -- the HIF exchange needs;
/// implemented by the live [`Link`], faked in tests.
pub trait ModuleBus: Registers {
    fn read_block(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), SpiError>;
    fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), SpiError>;
}

impl<S: SpiBus> ModuleBus for Link<S> {
    fn read_block(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), SpiError> {
        Link::read_block(self, addr, buf)
    }
    fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), SpiError> {
        Link::write_block(self, addr, data)
    }
}

/// A HIF send/receive failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HifError {
    Spi(SpiError),
    /// The message (header + payload) exceeds [`MAX_MESSAGE`].
    TooLong { length: usize },
    /// The firmware never allocated a receive buffer for the announced message.
    AllocTimeout,
    /// The written message did not read back intact (CRC framing is off after init, so a
    /// noisy transport corrupts silently -- this is the integrity net; `offset` is the first
    /// differing byte within the failed block).
    WriteVerify { offset: usize },
}

impl From<SpiError> for HifError {
    fn from(error: SpiError) -> Self {
        HifError::Spi(error)
    }
}

/// A received HIF message's identity: the payload (`length` bytes) sits in module memory at
/// `address` -- block-read it (fully or piecewise, e.g. a socket recv draining into a small
/// buffer), then acknowledge with [`set_receive_done`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HifEvent {
    pub group: u8,
    pub opcode: u8,
    pub length: u16,
    pub address: u32,
}

/// Sends one HIF message: `ctrl` is the request structure; `data`, when present, is a bulk
/// payload placed `data_offset` bytes after the header (socket sends use this split).
pub fn send<B: ModuleBus>(
    bus: &mut B,
    group: u8,
    opcode: u8,
    ctrl: &[u8],
    data: Option<(&[u8], u16)>,
) -> Result<(), HifError> {
    send_with(bus, group, opcode, ctrl, data, true)
}

/// [`send`] with the integrity trade exposed: `verify` = false skips the read-back check,
/// halving the transfer time on a slow transport -- for latency-critical sends whose payload
/// tolerates corruption (an app-data byte reaches the wire either way; a credential does not).
pub fn send_with<B: ModuleBus>(
    bus: &mut B,
    group: u8,
    opcode: u8,
    ctrl: &[u8],
    data: Option<(&[u8], u16)>,
    verify: bool,
) -> Result<(), HifError> {
    let body = match data {
        Some((payload, offset)) => usize::from(offset) + payload.len(),
        None => ctrl.len(),
    };
    let length = usize::from(HEADER_LEN) + body;
    if length > usize::from(MAX_MESSAGE) {
        return Err(HifError::TooLong { length });
    }

    let announce = u32::from(group) | (u32::from(opcode) << 8) | ((length as u32) << 16);
    bus.write_reg(NMI_STATE, announce)?;
    bus.write_reg(WIFI_HOST_RCV_CTRL_2, 1 << 1)?;

    let mut address = 0;
    for _ in 0..ALLOC_RETRIES {
        if bus.read_reg(WIFI_HOST_RCV_CTRL_2)? & (1 << 1) == 0 {
            address = bus.read_reg(WIFI_HOST_RCV_CTRL_4)?;
            break;
        }
    }
    if address == 0 {
        return Err(HifError::AllocTimeout);
    }

    let header = [
        group,
        opcode & !OPCODE_DATA_BIT,
        length as u8,
        (length >> 8) as u8,
        0,
        0,
        0,
        0,
    ];
    let write = if verify { write_verified } else { write_plain };
    if ctrl.is_empty() || ctrl.len() >= 4 {
        write(bus, address, &header)?;
        if !ctrl.is_empty() {
            write(bus, address + u32::from(HEADER_LEN), ctrl)?;
        }
    } else {
        let mut combined = [0u8; HEADER_LEN as usize + 4];
        combined[..HEADER_LEN as usize].copy_from_slice(&header);
        combined[HEADER_LEN as usize..HEADER_LEN as usize + ctrl.len()].copy_from_slice(ctrl);
        write(bus, address, &combined[..HEADER_LEN as usize + ctrl.len()])?;
    }
    if let Some((payload, offset)) = data {
        let target = address + u32::from(HEADER_LEN) + u32::from(offset);
        if payload.is_empty() {
        } else if payload.len() >= 4 {
            write(bus, target, payload)?;
        } else {
            let start = target - (4 - payload.len() as u32);
            let mut window = [0u8; 4];
            bus.read_block(start, &mut window)?;
            window[4 - payload.len()..].copy_from_slice(payload);
            write(bus, start, &window)?;
        }
    }

    bus.write_reg(WIFI_HOST_RCV_CTRL_3, (address << 2) | (1 << 1))?;
    Ok(())
}

/// Block-write with read-back verification: CRC framing is disabled after the boot handshake,
/// so a marginal transport (the SWD bit-bang especially) can corrupt a long write silently --
/// and a corrupted credential fails authentication with no other symptom. Verification happens
/// BEFORE the doorbell, while the buffer is still the host's.
fn write_verified<B: ModuleBus>(bus: &mut B, address: u32, data: &[u8]) -> Result<(), HifError> {
    bus.write_block(address, data)?;
    let mut check = [0u8; MAX_MESSAGE as usize];
    let check = &mut check[..data.len()];
    bus.read_block(address, check)?;
    if let Some(offset) = (0..data.len()).find(|&i| check[i] != data[i]) {
        return Err(HifError::WriteVerify { offset });
    }
    Ok(())
}

/// The unverified variant [`send_with`] selects for latency-critical payloads.
fn write_plain<B: ModuleBus>(bus: &mut B, address: u32, data: &[u8]) -> Result<(), HifError> {
    bus.write_block(address, data)?;
    Ok(())
}

/// Polls for a pending firmware message (the interrupt-status half of the reference's ISR --
/// callable on IRQN's edge or as a plain poll): when one is waiting, clears the interrupt and
/// returns its identity, header already consumed.
pub fn poll_event<B: ModuleBus>(bus: &mut B) -> Result<Option<HifEvent>, HifError> {
    let ctrl0 = bus.read_reg(WIFI_HOST_RCV_CTRL_0)?;
    if ctrl0 & 1 == 0 {
        return Ok(None);
    }
    let size = (ctrl0 >> 2) & 0xfff;
    if size == 0 {
        return Ok(None);
    }
    bus.write_reg(WIFI_HOST_RCV_CTRL_0, ctrl0 & !1)?;
    let address = bus.read_reg(WIFI_HOST_RCV_CTRL_1)?;
    let mut header = [0u8; 4];
    bus.read_block(address, &mut header)?;
    Ok(Some(HifEvent {
        group: header[0],
        opcode: header[1],
        length: u16::from_le_bytes([header[2], header[3]]),
        address,
    }))
}

/// Acknowledges a consumed message: the firmware may reuse its buffer. Call after every
/// [`poll_event`] hit, once the payload has been read.
pub fn set_receive_done<B: ModuleBus>(bus: &mut B) -> Result<(), HifError> {
    let ctrl0 = bus.read_reg(WIFI_HOST_RCV_CTRL_0)?;
    bus.write_reg(WIFI_HOST_RCV_CTRL_0, ctrl0 | (1 << 1))?;
    Ok(())
}

/// Test scaffolding shared with the wifi module's unit tests: a fake module whose registers
/// behave per the send/receive handshake (the allocation doorbell self-clears) and whose block
/// transfers land in a sparse memory.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::boot::Registers;
    use crate::spi::SpiError;
    use alloc::collections::BTreeMap;

    #[derive(Default)]
    pub(crate) struct FakeModule {
        pub(crate) regs: BTreeMap<u32, u32>,
        pub(crate) memory: BTreeMap<u32, u8>,
    }

    impl FakeModule {
        /// A fake whose next `send` is granted a message buffer at `address`.
        pub(crate) fn with_alloc(address: u32) -> Self {
            let mut module = Self::default();
            module.regs.insert(WIFI_HOST_RCV_CTRL_4, address);
            module
        }
    }

    impl Registers for FakeModule {
        fn read_reg(&mut self, addr: u32) -> Result<u32, SpiError> {
            if addr == WIFI_HOST_RCV_CTRL_2 {
                return Ok(0);
            }
            Ok(*self.regs.get(&addr).unwrap_or(&0))
        }
        fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), SpiError> {
            self.regs.insert(addr, value);
            Ok(())
        }
    }

    impl ModuleBus for FakeModule {
        fn read_block(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), SpiError> {
            for (offset, slot) in buf.iter_mut().enumerate() {
                *slot = *self.memory.get(&(addr + offset as u32)).unwrap_or(&0);
            }
            Ok(())
        }
        fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), SpiError> {
            for (offset, byte) in data.iter().enumerate() {
                self.memory.insert(addr + offset as u32, *byte);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeModule;
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn send_writes_header_payload_and_doorbells() {
        let mut module = FakeModule::with_alloc(0x40000);
        send(&mut module, GROUP_WIFI, 0x28, &[0xaa, 0xbb, 0xcc], None).expect("send");
        assert_eq!(module.regs[&NMI_STATE], 1 | (0x28 << 8) | (11 << 16));
        let mut header = [0u8; 8];
        module.read_block(0x40000, &mut header).unwrap();
        assert_eq!(header, [GROUP_WIFI, 0x28, 11, 0, 0, 0, 0, 0]);
        let mut payload = [0u8; 3];
        module.read_block(0x40000 + 8, &mut payload).unwrap();
        assert_eq!(payload, [0xaa, 0xbb, 0xcc]);
        assert_eq!(module.regs[&WIFI_HOST_RCV_CTRL_3], (0x40000 << 2) | 2);
    }

    #[test]
    fn send_rejects_an_oversized_message() {
        let mut module = FakeModule::default();
        let big = [0u8; 1600];
        assert_eq!(
            send(&mut module, GROUP_IP, 1, &big, None),
            Err(HifError::TooLong { length: 1608 })
        );
    }

    #[test]
    fn poll_event_consumes_the_interrupt_and_reads_the_header() {
        let mut module = FakeModule::default();
        module.regs.insert(WIFI_HOST_RCV_CTRL_0, 1 | (16 << 2));
        module.regs.insert(WIFI_HOST_RCV_CTRL_1, 0x50000);
        module.write_block(0x50000, &[GROUP_WIFI, 0x2c, 16, 0]).unwrap();
        let event = poll_event(&mut module).expect("poll").expect("event");
        assert_eq!(
            event,
            HifEvent { group: GROUP_WIFI, opcode: 0x2c, length: 16, address: 0x50000 }
        );
        assert_eq!(module.regs[&WIFI_HOST_RCV_CTRL_0] & 1, 0);
        set_receive_done(&mut module).expect("done");
        assert_eq!(module.regs[&WIFI_HOST_RCV_CTRL_0] & 2, 2);
    }

    #[test]
    fn poll_event_is_none_when_idle() {
        let mut module = FakeModule::default();
        assert_eq!(poll_event(&mut module).expect("poll"), None);
        let _ = Vec::<u8>::new();
    }

    #[test]
    fn short_payload_rides_a_wider_write_and_preserves_the_gap() {
        let mut module = FakeModule::with_alloc(0x40000);
        module.write_block(0x40000 + 8 + 78, &[0xa5, 0x5a]).unwrap();
        let ctrl = [0u8; 16];
        send(&mut module, GROUP_IP, 0x45 | OPCODE_DATA_BIT, &ctrl, Some((b"hi", 80))).expect("send");
        let mut around = [0u8; 4];
        module.read_block(0x40000 + 8 + 78, &mut around).unwrap();
        assert_eq!(around, [0xa5, 0x5a, b'h', b'i']);
        assert_eq!(module.regs[&NMI_STATE] >> 16, 8 + 80 + 2);
    }

    #[test]
    fn short_ctrl_rides_the_header_write() {
        let mut module = FakeModule::with_alloc(0x40000);
        send(&mut module, GROUP_IP, 0x4a, &[b'a', b'b', 0], None).expect("send");
        let mut raw = [0u8; 11];
        module.read_block(0x40000, &mut raw).unwrap();
        assert_eq!(&raw[..8], &[GROUP_IP, 0x4a, 11, 0, 0, 0, 0, 0]);
        assert_eq!(&raw[8..], &[b'a', b'b', 0]);
    }
}
