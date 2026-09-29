//! The frame layer of the packet channel: the twelve-byte header at the
//! head of every function-2 frame in both directions, the credit window
//! the chip grants through it, the channels it multiplexes, the abort of a
//! bad frame, and the service pass that takes at most one frame per call.
//!
//! The header carries the frame's size and the size's bitwise complement,
//! a transmit sequence number, the channel in the low four bits of a byte,
//! a hint of the next frame's length, the offset of the payload from the
//! header's start, flow-control flags, and the credit: the sequence number
//! the host may not reach. The host transmits only while its own sequence
//! differs from the credit, and every valid header it receives moves the
//! credit; a twelve-byte frame with nothing after its header is a credit
//! update alone. The channels are 0 for control, 1 for events and 2 for
//! data. Every function-2 transfer, both ways, is at address 0: the chip
//! frames by the length at the head of the frame and ignores the address
//! (Infineon CYW43439 datasheet, section 12).

use crate::backplane::{Window, f1, frame_control};
use crate::cores::{SDIOD, sdiod};
use crate::data;
use crate::error::Refusal;
use crate::event::{self, Event, Parsed};
use crate::transport::{Func, Transport};

/// The header's length in bytes.
pub const HEADER_LEN: usize = 12;

/// The buffer a frame is built in and received into: the packet channel's
/// ceiling on both buses.
pub const FRAME_BUF: usize = 2048;

/// The channels, in the low four bits of the header's channel byte.
pub mod channel {
    /// The control channel.
    pub const CONTROL: u8 = 0;
    /// The event channel.
    pub const EVENT: u8 = 1;
    /// The data channel.
    pub const DATA: u8 = 2;
    /// The channel bits of the byte.
    pub const MASK: u8 = 0x0F;
}

/// The stage name of a frame the bus announced longer than the buffer.
pub const STAGE_FRAME_LENGTH: &str = "frame length";

/// The header of a function-2 frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The frame's length in bytes, the header included.
    pub size: u16,
    /// The transmit sequence number.
    pub sequence: u8,
    /// The channel byte; the channel is its low four bits.
    pub channel: u8,
    /// A hint of the next frame's length.
    pub next_length: u8,
    /// The payload's offset from the header's start, in bytes.
    pub data_offset: u8,
    /// Flow-control flags.
    pub flow_control: u8,
    /// The sequence number the host may not reach.
    pub credit: u8,
}

impl Header {
    /// A transmit header: the size, the sequence, the channel and the
    /// payload's offset; the hint, the flags and the credit zero.
    pub const fn transmit(size: u16, sequence: u8, channel: u8, data_offset: u8) -> Self {
        Header {
            size,
            sequence,
            channel,
            next_length: 0,
            data_offset,
            flow_control: 0,
            credit: 0,
        }
    }

    /// The channel: the low four bits of the channel byte.
    pub const fn channel_id(&self) -> u8 {
        self.channel & channel::MASK
    }

    /// Write the twelve bytes at the head of `into`: the size and its
    /// complement little-endian, the six single bytes, two bytes of zero
    /// padding.
    pub fn write(&self, into: &mut [u8]) {
        into[0..2].copy_from_slice(&self.size.to_le_bytes());
        into[2..4].copy_from_slice(&(!self.size).to_le_bytes());
        into[4] = self.sequence;
        into[5] = self.channel;
        into[6] = self.next_length;
        into[7] = self.data_offset;
        into[8] = self.flow_control;
        into[9] = self.credit;
        into[10] = 0;
        into[11] = 0;
    }
}

/// What is wrong with a received frame's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Fewer bytes than the header.
    Short,
    /// The complement does not check.
    Checksum,
    /// The size is under the header's length or over the bytes read.
    Size,
    /// The payload's offset is under the header's length or over the size.
    Offset,
}

/// The reading of a received frame's head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inspection {
    /// The size field reads zero: no frame (the announcement raced the
    /// chip's FIFO).
    Empty,
    /// A header that fails a check.
    Bad(Fault),
    /// A valid header.
    Frame(Header),
}

/// Read a received frame's head: the exact complement, the size between the
/// header's length and the bytes read, and -- for a frame longer than its
/// header -- the payload's offset between the header's length and the size.
pub fn inspect(frame: &[u8]) -> Inspection {
    if frame.len() >= 2 && frame[0] == 0 && frame[1] == 0 {
        return Inspection::Empty;
    }
    if frame.len() < HEADER_LEN {
        return Inspection::Bad(Fault::Short);
    }
    let size = u16::from_le_bytes([frame[0], frame[1]]);
    let check = u16::from_le_bytes([frame[2], frame[3]]);
    if size ^ check != 0xFFFF {
        return Inspection::Bad(Fault::Checksum);
    }
    let len = usize::from(size);
    if !(HEADER_LEN..=frame.len()).contains(&len) {
        return Inspection::Bad(Fault::Size);
    }
    let header = Header {
        size,
        sequence: frame[4],
        channel: frame[5],
        next_length: frame[6],
        data_offset: frame[7],
        flow_control: frame[8],
        credit: frame[9],
    };
    let offset = usize::from(header.data_offset);
    if len > HEADER_LEN && !(HEADER_LEN..=len).contains(&offset) {
        return Inspection::Bad(Fault::Offset);
    }
    Inspection::Frame(header)
}

/// The credit window: the host's next transmit sequence number and the
/// sequence number the chip has said the host may not reach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Credit {
    tx_seq: u8,
    max_seq: u8,
}

impl Credit {
    /// A closed window: both sequences zero.
    pub const fn new() -> Self {
        Credit {
            tx_seq: 0,
            max_seq: 0,
        }
    }

    /// Whether the host may transmit.
    pub const fn open(&self) -> bool {
        self.tx_seq != self.max_seq
    }

    /// Take the credit a received header carries.
    pub fn take(&mut self, credit: u8) {
        self.max_seq = credit;
    }

    /// The sequence number for the next transmitted frame, the counter
    /// moved past it (wrapping at eight bits).
    pub fn issue(&mut self) -> u8 {
        let sequence = self.tx_seq;
        self.tx_seq = self.tx_seq.wrapping_add(1);
        sequence
    }

    /// Give back the sequence number just issued: the frame it was issued
    /// for was not written.
    pub fn retract(&mut self) {
        self.tx_seq = self.tx_seq.wrapping_sub(1);
    }

    /// The window: the next transmit sequence and the credit.
    pub const fn window(&self) -> (u8, u8) {
        (self.tx_seq, self.max_seq)
    }

    /// The window as one word: the credit in bits 15:8, the next transmit
    /// sequence in bits 7:0.
    pub const fn word(&self) -> u32 {
        ((self.max_seq as u32) << 8) | self.tx_seq as u32
    }
}

/// The frame counters; each wraps after 2^32.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frames {
    /// Frames read from the channel, whatever their fate.
    pub received: u32,
    /// Frames written to the channel.
    pub sent: u32,
    /// Frames taken from the event channel, whatever their fate.
    pub events: u32,
    /// Event frames the walk dropped: a wrong ethertype, a wrong OUI, a
    /// header cut short, a number past the count.
    pub malformed: u32,
    /// Frames dropped: a control frame that is not the outstanding reply,
    /// a malformed reply, a frame on an unknown channel.
    pub dropped: u32,
    /// Frames whose header failed and were aborted.
    pub aborted: u32,
    /// Announcements of a frame that read as no frame.
    pub empty: u32,
    /// Frames taken from the data channel, whatever their fate.
    pub data_received: u32,
    /// Data frames written to the channel.
    pub data_sent: u32,
    /// Data frames dropped: malformed, over the unit, or received while the
    /// link was not up.
    pub data_dropped: u32,
}

/// What a service pass handed back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    /// No frame was taken.
    Nothing,
    /// A frame was taken and consumed.
    Consumed,
    /// A control frame waits in the buffer, its payload at the header's
    /// data offset.
    Control(Header),
    /// An event frame was taken and walked; its payload waits in the
    /// buffer.
    Event(Event),
    /// A data frame was taken and walked; its Ethernet frame of `len`
    /// bytes waits in the buffer.
    Data {
        /// The Ethernet frame's length.
        len: usize,
    },
}

/// The frame layer's state: the credit window, the counters, whether the
/// chip's mailbox interrupt is the bus's frame indication, and where the
/// last event's payload or the last data frame's Ethernet frame lies in
/// the buffer.
#[derive(Clone, Copy, Debug)]
pub struct Layer {
    /// The credit window.
    pub credit: Credit,
    /// The counters.
    pub frames: Frames,
    mailbox: bool,
    event: Option<(usize, usize)>,
    data: Option<(usize, usize)>,
}

impl Layer {
    /// A closed window, zero counters, no mailbox.
    pub const fn new() -> Self {
        Layer {
            credit: Credit::new(),
            frames: Frames {
                received: 0,
                sent: 0,
                events: 0,
                malformed: 0,
                dropped: 0,
                aborted: 0,
                empty: 0,
                data_received: 0,
                data_sent: 0,
                data_dropped: 0,
            },
            mailbox: false,
            event: None,
            data: None,
        }
    }

    /// The offset and length in the buffer of the payload of the event
    /// the last pass walked, if it walked one; forgotten by the next pass.
    pub(crate) const fn last_event(&self) -> Option<(usize, usize)> {
        self.event
    }

    /// The offset and length in the buffer of the Ethernet frame of the
    /// data frame the last pass walked, if it walked one; forgotten by the
    /// next pass.
    pub(crate) const fn last_data(&self) -> Option<(usize, usize)> {
        self.data
    }

    /// Forget the last data frame's Ethernet frame: it was dropped.
    pub(crate) fn forget_data(&mut self) {
        self.data = None;
    }

    /// Remember whether the chip's mailbox interrupt is the bus's frame
    /// indication, as the transport answered at the interrupt setup.
    pub fn set_mailbox(&mut self, mailbox: bool) {
        self.mailbox = mailbox;
    }

    /// Whether the chip's mailbox interrupt is the bus's frame indication.
    pub const fn mailbox(&self) -> bool {
        self.mailbox
    }

    /// One service pass: the last event's payload forgotten; the bus-level
    /// interrupt taken; when the chip's mailbox interrupt is the bus's
    /// indication and the latch was not zero, the mailbox interrupt status
    /// read and written back; then at most one frame read and dispatched --
    /// the credit taken from a valid header, a twelve-byte frame consumed,
    /// a control frame handed back, an event frame walked and handed back
    /// (an empty event consumed; one the walk drops counted), a data frame
    /// walked to its Ethernet frame and handed back (one the walk drops
    /// counted), an unknown channel dropped and counted, a bad header
    /// aborted and counted.
    pub fn service<T: Transport>(
        &mut self,
        bus: &mut T,
        window: &mut Window,
        buf: &mut [u8],
    ) -> Result<Served, Refusal> {
        self.event = None;
        self.data = None;
        let latch = bus.take_interrupt()?;
        if self.mailbox && latch != 0 {
            let status = window.read32(bus, SDIOD + sdiod::INTSTATUS)?;
            if status != 0 {
                window.write32(bus, SDIOD + sdiod::INTSTATUS, status)?;
            }
        }
        let len = match bus.f2_frame_available()? {
            None => return Ok(Served::Nothing),
            Some(0) => {
                self.frames.empty += 1;
                return Ok(Served::Nothing);
            }
            Some(len) => len,
        };
        if len > buf.len() {
            return Err(Refusal::new(STAGE_FRAME_LENGTH, len as u32));
        }
        let n = bus.f2_read(len, buf)?;
        self.frames.received += 1;
        match inspect(&buf[..n]) {
            Inspection::Empty => {
                self.frames.empty += 1;
                Ok(Served::Nothing)
            }
            Inspection::Bad(_) => {
                abort(bus)?;
                self.frames.aborted += 1;
                Ok(Served::Nothing)
            }
            Inspection::Frame(header) => {
                self.credit.take(header.credit);
                if usize::from(header.size) == HEADER_LEN {
                    return Ok(Served::Consumed);
                }
                match header.channel_id() {
                    channel::CONTROL => Ok(Served::Control(header)),
                    channel::EVENT => {
                        self.frames.events += 1;
                        let frame = &buf[..usize::from(header.size)];
                        match event::parse(frame, usize::from(header.data_offset)) {
                            Parsed::Event { event, at } => {
                                self.event = Some((at, event.len));
                                Ok(Served::Event(event))
                            }
                            Parsed::Empty => Ok(Served::Consumed),
                            Parsed::Dropped(_) => {
                                self.frames.malformed += 1;
                                Ok(Served::Consumed)
                            }
                        }
                    }
                    channel::DATA => {
                        self.frames.data_received += 1;
                        let frame = &buf[..usize::from(header.size)];
                        match data::parse(frame, usize::from(header.data_offset)) {
                            Ok((at, len)) => {
                                self.data = Some((at, len));
                                Ok(Served::Data { len })
                            }
                            Err(_) => {
                                self.frames.data_dropped += 1;
                                Ok(Served::Consumed)
                            }
                        }
                    }
                    _ => {
                        self.frames.dropped += 1;
                        Ok(Served::Consumed)
                    }
                }
            }
        }
    }
}

impl Default for Layer {
    fn default() -> Self {
        Self::new()
    }
}

/// The abort of a bad frame: the transport's bus-level half, then the
/// read-frame-terminate bit written to the frame control register on
/// function 1.
pub fn abort<T: Transport>(bus: &mut T) -> Result<(), Refusal> {
    bus.abort_f2()?;
    bus.write_direct(Func::F1, f1::FRAME_CONTROL, frame_control::RF_TERM)
}
