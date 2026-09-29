//! The data plane: the data frames on the packet channel's third channel,
//! carrying the caller's Ethernet frames both ways -- the four-byte BDC
//! header behind the frame header, the two alignment bytes of a
//! transmitted frame, the walk through a received frame to its Ethernet
//! frame, and the one frame the driver holds for transmission.
//!
//! A transmitted data frame is the twelve-byte frame header with its
//! payload offset 14, two zero bytes so the Ethernet frame starts on an
//! even offset, the BDC header (the protocol version in the high nibble
//! of its flags, the priority, the interface index, a data offset of 0)
//! and the Ethernet frame at offset 18. A received data frame carries the
//! chip's own payload offset in its frame header, the BDC header there, a
//! padding of the BDC header's data offset in 32-bit words, and the
//! Ethernet frame to the frame's end. The Ethernet frame is at most 1,514
//! bytes both ways, the unit the device adapter states: a 14-byte header
//! and 1,500 bytes of payload. The driver writes the frame it holds under
//! the credit window with a sequence number spent only when the chip took
//! the frame, so a frame held across other sends never carries a stale
//! number.

use crate::error::Refusal;
use crate::frame::{HEADER_LEN, Header, Layer, channel};
use crate::transport::Transport;

/// The BDC header's length.
pub const BDC_HEADER_LEN: usize = 4;
/// The alignment padding between the frame header and the BDC header of a
/// transmitted data frame.
pub const ALIGN_PAD: usize = 2;
/// The payload offset a transmitted data frame's header names: the frame
/// header and the alignment padding.
pub const TRANSMIT_OFFSET: usize = HEADER_LEN + ALIGN_PAD;
/// The bytes before the Ethernet frame of a transmitted data frame.
pub const HEAD: usize = TRANSMIT_OFFSET + BDC_HEADER_LEN;
/// An Ethernet frame's header: the destination, the source, the type.
pub const ETHERNET_HEADER_LEN: usize = 14;
/// The longest Ethernet frame both ways: the header and 1,500 bytes of
/// payload.
pub const ETHERNET_MAX: usize = 1514;
/// The transmit buffer: the headers and the longest Ethernet frame.
pub const TRANSMIT_BUF: usize = HEAD + ETHERNET_MAX;

/// The BDC header's fields on a transmitted frame.
pub mod bdc {
    /// The flags byte: the protocol version, 2, in the high nibble.
    pub const VERSION: u8 = 0x20;
    /// The priority.
    pub const PRIORITY: u8 = 0;
    /// The second flags byte: the station interface's index.
    pub const IFACE_STATION: u8 = 0;
}

/// The BDC header: the flags, the priority, the second flags byte carrying
/// the interface index, and the data offset in 32-bit words from the
/// header's end to the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BdcHeader {
    /// The flags: the protocol version in the high nibble.
    pub flags: u8,
    /// The priority.
    pub priority: u8,
    /// The second flags byte: the interface index.
    pub flags2: u8,
    /// The padding after the header, in 32-bit words.
    pub data_offset: u8,
}

impl BdcHeader {
    /// The header of a transmitted frame: the version, the priority 0, the
    /// station interface, no padding.
    pub const TRANSMIT: BdcHeader = BdcHeader {
        flags: bdc::VERSION,
        priority: bdc::PRIORITY,
        flags2: bdc::IFACE_STATION,
        data_offset: 0,
    };

    /// Write the four bytes at the head of `into`.
    pub fn write(&self, into: &mut [u8]) {
        into[0] = self.flags;
        into[1] = self.priority;
        into[2] = self.flags2;
        into[3] = self.data_offset;
    }

    /// The header at the head of `bytes`, or `None` for fewer than four.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < BDC_HEADER_LEN {
            return None;
        }
        Some(BdcHeader {
            flags: bytes[0],
            priority: bytes[1],
            flags2: bytes[2],
            data_offset: bytes[3],
        })
    }

    /// The padding after the header, in bytes.
    pub const fn padding(&self) -> usize {
        4 * self.data_offset as usize
    }
}

/// What is wrong with a received data frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The frame ends before the BDC header, its padding or an Ethernet
    /// header does.
    Short,
    /// The Ethernet frame is longer than the unit.
    Length,
}

/// Walk a received data frame from its frame header's payload offset: the
/// BDC header read, its padding skipped, the Ethernet frame the rest of the
/// frame. The Ethernet frame's offset in the frame and its length, or the
/// fault.
pub fn parse(frame: &[u8], data_offset: usize) -> Result<(usize, usize), Fault> {
    let body = frame.get(data_offset..).ok_or(Fault::Short)?;
    let header = BdcHeader::parse(body).ok_or(Fault::Short)?;
    let at = BDC_HEADER_LEN + header.padding();
    let len = body.len().checked_sub(at).ok_or(Fault::Short)?;
    if len < ETHERNET_HEADER_LEN {
        return Err(Fault::Short);
    }
    if len > ETHERNET_MAX {
        return Err(Fault::Length);
    }
    Ok((data_offset + at, len))
}

/// Lay the eighteen bytes of headers down over an Ethernet frame of `len`
/// bytes already at offset 18 of `buf`, with the transmit sequence; returns
/// the frame's length.
pub fn head(buf: &mut [u8], sequence: u8, len: usize) -> usize {
    let total = HEAD + len;
    Header::transmit(total as u16, sequence, channel::DATA, TRANSMIT_OFFSET as u8).write(buf);
    buf[HEADER_LEN..TRANSMIT_OFFSET].fill(0);
    BdcHeader::TRANSMIT.write(&mut buf[TRANSMIT_OFFSET..HEAD]);
    total
}

/// Whether an Ethernet frame of `len` bytes can be staged: at least a
/// header, at most the unit.
pub const fn fits(len: usize) -> bool {
    len >= ETHERNET_HEADER_LEN && len <= ETHERNET_MAX
}

/// Write the staged frame of `len` Ethernet bytes: the headers laid down
/// with a sequence issued from the window; accepted, the frame counts as
/// sent and `true` comes back; not accepted, the sequence is retracted and
/// the frame kept for a later attempt.
pub(crate) fn write<T: Transport>(
    bus: &mut T,
    buf: &mut [u8],
    len: usize,
    layer: &mut Layer,
) -> Result<bool, Refusal> {
    let sequence = layer.credit.issue();
    let total = head(buf, sequence, len);
    if bus.f2_write(&buf[..total])? {
        layer.frames.sent += 1;
        layer.frames.data_sent += 1;
        Ok(true)
    } else {
        layer.credit.retract();
        Ok(false)
    }
}
