//! Following a program's console ring in a board's RAM over a debug probe, without halting the
//! board.
//!
//! An image that keeps a console ring writes every byte its console carries into a block of RAM,
//! and, once the program has ended, how it ended. A reader holding a probe looks at that block
//! while the program runs and turns successive looks into what the program printed, what it lost,
//! and its end. The image's ELF names the block: `__lamella_console_log` in a Lamella image, or
//! `__rhu_console_log` in an image Lamella RHU built ([`ring_in`]).
//!
//! The ring's layout, in little-endian words: a magic, the buffer's size, the count of bytes written
//! since the board started, and the exit code, then the buffer. The magic is `RLOG` while the
//! program runs and an end magic once it has ended, one for each way it can end, with the program's
//! code beside it, whole. The image lands each byte at the count modulo the size and then moves the
//! count on, and never waits for a reader: once the count passes the size, the oldest bytes are
//! gone. At every start it clears the magic first and writes it last, so a ring a reset left behind
//! never reads as the new run's once the new run has begun.
//!
//! A [`RingReader`] keeps only what it has followed; the memory it reads is passed to each
//! [`RingReader::poll`], so the reader can be kept between looks by a caller that holds its probe
//! only for the length of one.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use lamella_elf::symbols::DataObject;

/// The symbol a Lamella image's ELF names its console ring by.
pub const LAMELLA_RING: &str = "__lamella_console_log";

/// The symbol a Lamella RHU image's ELF names its console ring by.
pub const RHU_RING: &str = "__rhu_console_log";

/// `RLOG`, the magic while the program runs, as its four bytes read as a little-endian word.
pub const RUNNING: u32 = 0x474f_4c52;

/// The magics that take the place of [`RUNNING`] once a program has ended, one for each way it can
/// end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndMagics {
    /// The program returned, or exited, with the code beside it.
    pub returned: u32,
    /// The program was aborted, with the code beside it.
    pub aborted: u32,
}

/// The end magics the boards' images write: `RRET` when the program returned, and `RABT` when it
/// was aborted.
pub const END_MAGICS: EndMagics = EndMagics { returned: 0x5445_5252, aborted: 0x5442_4152 };

/// The header's length: four words.
pub const HEADER: u32 = 16;

/// The largest buffer believed of a ring its image gives no size.
const LARGEST: u32 = 1 << 20;

/// How a program on a board ended, as the board reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramEnd {
    /// It returned, or exited, with this code.
    Returned(i32),
    /// It was aborted -- by an exception nothing caught, by a fail-fast, or by its kernel -- and
    /// the board gives this code for it.
    Aborted(i32),
}

/// What a look at the ring found since the one before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    /// Bytes the board's console carried, as the board wrote them.
    Out(Vec<u8>),
    /// This many bytes of the console were overwritten on the board before they could be read.
    Lost(u64),
    /// The board started again, and the program with it.
    Restarted,
    /// The program ended.
    Ended(ProgramEnd),
}

/// Reading a board's memory through a probe, without halting the board.
pub trait TargetMemory {
    /// Fills `into` from the board's memory at `address`, which need not be word-aligned.
    ///
    /// # Errors
    /// A read the probe could not make.
    fn read(&mut self, address: u32, into: &mut [u8]) -> Result<(), String>;
}

/// Why an ELF gives no console ring to follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoRing {
    /// It names neither [`LAMELLA_RING`] nor [`RHU_RING`].
    Neither,
    /// It names both, so which one its program writes cannot be told from it.
    Both,
    /// It names one, `symbol`, with `size` bytes: too few for the ring's header.
    TooSmall {
        /// The symbol it names.
        symbol: &'static str,
        /// The bytes the symbol covers.
        size: u32,
    },
}

/// The console ring `elf` names: the symbol it names it by, and where it is and how many bytes it
/// covers.
///
/// One ring or none: a file naming both cannot say which one its program writes, and following the
/// other would wait on a ring nothing writes. A ring whose size is given must hold its header; one
/// given no size is bounded by its header's own claim when it is read.
///
/// # Errors
/// [`NoRing`], saying which way there is no ring to follow.
pub fn ring_in(elf: &[u8]) -> Result<(&'static str, DataObject), NoRing> {
    let (symbol, ring) = match (
        lamella_elf::symbols::object(elf, LAMELLA_RING),
        lamella_elf::symbols::object(elf, RHU_RING),
    ) {
        (Some(ring), None) => (LAMELLA_RING, ring),
        (None, Some(ring)) => (RHU_RING, ring),
        (None, None) => return Err(NoRing::Neither),
        (Some(_), Some(_)) => return Err(NoRing::Both),
    };
    if ring.size != 0 && ring.size <= HEADER {
        return Err(NoRing::TooSmall { symbol, size: ring.size });
    }
    Ok((symbol, ring))
}

/// What the magic says of the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// It is running.
    Running,
    /// It has ended, this way.
    Ended(ProgramEnd),
}

/// The ring's header, as one read found it.
struct Header {
    state: State,
    size: u32,
    written: u32,
}

/// A program's console, followed through successive looks at its ring.
#[derive(Clone, Debug)]
pub struct RingReader {
    /// Where the ring starts.
    at: u32,
    /// How many bytes the image gives the ring, its header included; 0 when it does not say.
    span: u32,
    /// The magics that say the program ended.
    ends: EndMagics,
    /// The count of bytes already delivered.
    consumed: u32,
    /// How many more looks may find no ring before that is an error.
    patience: u32,
    /// How many looks a missing ring is waited through.
    patience_at_start: u32,
}

impl RingReader {
    /// A reader of the ring `ring` names, which knows the program ended by `ends` and waits through
    /// `patience` looks for a ring the board has not set up yet.
    #[must_use]
    pub fn new(ring: DataObject, ends: EndMagics, patience: u32) -> Self {
        RingReader { at: ring.address, span: ring.size, ends, consumed: 0, patience, patience_at_start: patience }
    }

    /// What arrived since the last look, read through `memory`; empty when nothing did.
    ///
    /// The text is untagged: the ring holds the board's streams as they were written, so all of it
    /// comes back in order, and the program's end comes after its last bytes.
    ///
    /// # Errors
    /// A ring the board has not set up within the patience the reader was given, a header that
    /// claims a buffer the image does not give the ring, and a read the probe could not make.
    pub fn poll(&mut self, memory: &mut dyn TargetMemory) -> Result<Vec<Seen>, String> {
        let Some(mut header) = self.header(memory)? else {
            if self.patience == 0 {
                return Err(format!(
                    "there is no console ring at {:#010x}: the board has not set one up there, so it is \
                     not running\nthe image that was written, or the ELF the address came from is not \
                     that image's.",
                    self.at
                ));
            }
            self.patience -= 1;
            return Ok(Vec::new());
        };
        self.patience = self.patience_at_start;
        if let State::Ended(_) = header.state
            && let Some(again) = self.header(memory)?
            && matches!(again.state, State::Ended(_))
        {
            header = again;
        }
        let mut seen = Vec::new();
        self.drain(memory, &header, &mut seen)?;
        if let State::Ended(end) = header.state {
            seen.push(Seen::Ended(end));
        }
        Ok(seen)
    }

    /// One read of the header: `None` while the board has not set the ring up.
    fn header(&self, memory: &mut dyn TargetMemory) -> Result<Option<Header>, String> {
        let mut bytes = [0u8; HEADER as usize];
        memory.read(self.at, &mut bytes)?;
        let word = |index: usize| {
            u32::from_le_bytes([bytes[index * 4], bytes[index * 4 + 1], bytes[index * 4 + 2], bytes[index * 4 + 3]])
        };
        let code = word(3) as i32;
        let state = match word(0) {
            RUNNING => State::Running,
            magic if magic == self.ends.returned => State::Ended(ProgramEnd::Returned(code)),
            magic if magic == self.ends.aborted => State::Ended(ProgramEnd::Aborted(code)),
            _ => return Ok(None),
        };
        let size = word(1);
        let fits = match self.span {
            0 => size <= LARGEST,
            span => u64::from(size) + u64::from(HEADER) <= u64::from(span),
        };
        if size == 0 || !fits {
            return Err(format!(
                "the console ring at {:#010x} claims a buffer of {size} bytes, which is not the ring the \
                 image gives it.",
                self.at
            ));
        }
        Ok(Some(Header { state, size, written: word(2) }))
    }

    /// The count of bytes written, read alone.
    fn written_now(&self, memory: &mut dyn TargetMemory) -> Result<u32, String> {
        let mut bytes = [0u8; 4];
        memory.read(self.at.wrapping_add(8), &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Everything written since the last drain, onto `seen`.
    fn drain(&mut self, memory: &mut dyn TargetMemory, header: &Header, seen: &mut Vec<Seen>) -> Result<(), String> {
        if header.written < self.consumed {
            seen.push(Seen::Restarted);
            self.consumed = 0;
        }
        let pending = header.written - self.consumed;
        if pending == 0 {
            return Ok(());
        }
        let size = header.size;
        let mut from = self.consumed;
        let mut lost = 0;
        if pending > size {
            lost = pending - size;
            from = header.written - size;
        }
        let length = header.written - from;
        let mut bytes = vec![0u8; length as usize];
        let offset = from % size;
        let first = length.min(size - offset);
        let buffer = self.at.wrapping_add(HEADER);
        memory.read(buffer.wrapping_add(offset), &mut bytes[..first as usize])?;
        if first < length {
            memory.read(buffer, &mut bytes[first as usize..])?;
        }
        let after = self.written_now(memory)?;
        let overwritten = after.saturating_sub(size).saturating_sub(from).min(length);
        lost += overwritten;
        if lost > 0 {
            seen.push(Seen::Lost(u64::from(lost)));
        }
        bytes.drain(..overwritten as usize);
        self.consumed = header.written;
        if !bytes.is_empty() {
            seen.push(Seen::Out(bytes));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
