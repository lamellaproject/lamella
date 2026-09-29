//! The recorder: a transport that wraps any other, delegates every
//! operation and logs it, so the operations the core performed over one
//! bus can be compared with those over another and with a script at the
//! transport level.

use crate::clock::Micros;
use crate::error::Refusal;
use crate::transport::{Attach, Func, Transport, Tune};

/// The kind of a logged operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The transport reported `Ready`.
    Attach,
    /// A direct read.
    ReadDirect,
    /// A direct write.
    WriteDirect,
    /// An extended read.
    ReadExtended,
    /// An extended write.
    WriteExtended,
    /// A status read.
    Status,
    /// An interrupt take.
    TakeInterrupt,
    /// The bus-level abort of function 2.
    AbortF2,
    /// The availability query.
    F2Available,
    /// A frame read.
    F2Read,
    /// A frame write.
    F2Write,
    /// The packet channel's readiness query.
    F2Ready,
    /// The wake-on-command control.
    WakeOnCommand,
    /// The tuning hook reported `Done`.
    Tune,
    /// The packet channel's interrupt setup; the word is its answer.
    F2InterruptSetup,
}

/// One logged operation: its kind, function, address, addressing and
/// length, and the byte or the first four bytes as a little-endian word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Logged {
    /// The kind.
    pub kind: Kind,
    /// The function.
    pub func: Func,
    /// The address.
    pub addr: u32,
    /// Incrementing addressing.
    pub incr: bool,
    /// The length in bytes.
    pub len: usize,
    /// The byte, or the first four bytes as a little-endian word.
    pub word: u32,
}

impl Logged {
    /// A direct read or write of one byte.
    pub const fn direct(kind: Kind, func: Func, addr: u32, value: u8) -> Self {
        Logged {
            kind,
            func,
            addr,
            incr: true,
            len: 1,
            word: value as u32,
        }
    }

    /// An extended read or write, or a frame transfer.
    pub const fn extended(
        kind: Kind,
        func: Func,
        addr: u32,
        incr: bool,
        len: usize,
        word: u32,
    ) -> Self {
        Logged {
            kind,
            func,
            addr,
            incr,
            len,
            word,
        }
    }

    /// An operation with no function or address.
    pub const fn plain(kind: Kind, word: u32) -> Self {
        Logged {
            kind,
            func: Func::F0,
            addr: 0,
            incr: false,
            len: 0,
            word,
        }
    }
}

/// The first four bytes of a slice as a little-endian word, zero padded.
pub fn word_of(bytes: &[u8]) -> u32 {
    let mut word = [0u8; 4];
    let n = bytes.len().min(4);
    word[..n].copy_from_slice(&bytes[..n]);
    u32::from_le_bytes(word)
}

const CAPACITY: usize = 512;
const EMPTY: Logged = Logged::plain(Kind::Attach, 0);

/// A transport that logs what it is asked and passes it on.
#[derive(Debug)]
pub struct Recorder<T> {
    inner: T,
    log: [Logged; CAPACITY],
    n: usize,
    overflowed: bool,
}

impl<T> Recorder<T> {
    /// A recorder around `inner`.
    pub const fn new(inner: T) -> Self {
        Recorder {
            inner,
            log: [EMPTY; CAPACITY],
            n: 0,
            overflowed: false,
        }
    }

    /// The operations logged so far (the first 512).
    pub fn log(&self) -> &[Logged] {
        &self.log[..self.n]
    }

    /// Whether more operations happened than the log holds.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// The wrapped transport.
    pub fn inner(&self) -> &T {
        &self.inner
    }

    /// The wrapped transport, mutably.
    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Give the wrapped transport back.
    pub fn into_inner(self) -> T {
        self.inner
    }

    /// Forget the log.
    pub fn clear(&mut self) {
        self.n = 0;
        self.overflowed = false;
    }

    fn push(&mut self, entry: Logged) {
        if self.n < CAPACITY {
            self.log[self.n] = entry;
            self.n += 1;
        } else {
            self.overflowed = true;
        }
    }
}

impl<T: Transport> Transport for Recorder<T> {
    const F1_CHUNK: usize = T::F1_CHUNK;

    fn chip_id(&self) -> u16 {
        self.inner.chip_id()
    }

    fn attach(&mut self, now: Micros) -> Result<Attach, Refusal> {
        let step = self.inner.attach(now);
        if let Ok(Attach::Ready) = step {
            self.push(Logged::plain(Kind::Attach, 0));
        }
        step
    }

    fn read_direct(&mut self, func: Func, addr: u32) -> Result<u8, Refusal> {
        let value = self.inner.read_direct(func, addr)?;
        self.push(Logged::direct(Kind::ReadDirect, func, addr, value));
        Ok(value)
    }

    fn write_direct(&mut self, func: Func, addr: u32, value: u8) -> Result<(), Refusal> {
        self.inner.write_direct(func, addr, value)?;
        self.push(Logged::direct(Kind::WriteDirect, func, addr, value));
        Ok(())
    }

    fn read_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        buf: &mut [u8],
    ) -> Result<(), Refusal> {
        self.inner.read_extended(func, addr, incr, buf)?;
        self.push(Logged::extended(
            Kind::ReadExtended,
            func,
            addr,
            incr,
            buf.len(),
            word_of(buf),
        ));
        Ok(())
    }

    fn write_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        data: &[u8],
    ) -> Result<(), Refusal> {
        self.inner.write_extended(func, addr, incr, data)?;
        self.push(Logged::extended(
            Kind::WriteExtended,
            func,
            addr,
            incr,
            data.len(),
            word_of(data),
        ));
        Ok(())
    }

    fn status(&mut self) -> Result<u32, Refusal> {
        let value = self.inner.status()?;
        self.push(Logged::plain(Kind::Status, value));
        Ok(value)
    }

    fn take_interrupt(&mut self) -> Result<u16, Refusal> {
        let value = self.inner.take_interrupt()?;
        self.push(Logged::plain(Kind::TakeInterrupt, u32::from(value)));
        Ok(value)
    }

    fn abort_f2(&mut self) -> Result<(), Refusal> {
        self.inner.abort_f2()?;
        self.push(Logged::plain(Kind::AbortF2, 0));
        Ok(())
    }

    fn f2_frame_available(&mut self) -> Result<Option<usize>, Refusal> {
        let len = self.inner.f2_frame_available()?;
        self.push(Logged::plain(
            Kind::F2Available,
            len.map_or(0, |l| l as u32),
        ));
        Ok(len)
    }

    fn f2_read(&mut self, len: usize, frame: &mut [u8]) -> Result<usize, Refusal> {
        let n = self.inner.f2_read(len, frame)?;
        self.push(Logged::extended(
            Kind::F2Read,
            Func::F2,
            0,
            true,
            n,
            word_of(&frame[..n]),
        ));
        Ok(n)
    }

    fn f2_write(&mut self, frame: &[u8]) -> Result<bool, Refusal> {
        let accepted = self.inner.f2_write(frame)?;
        self.push(Logged::extended(
            Kind::F2Write,
            Func::F2,
            0,
            true,
            frame.len(),
            word_of(frame),
        ));
        Ok(accepted)
    }

    fn f2_ready(&mut self) -> Result<bool, Refusal> {
        let ready = self.inner.f2_ready()?;
        self.push(Logged::plain(Kind::F2Ready, u32::from(ready)));
        Ok(ready)
    }

    fn wake_on_command(&mut self) -> Result<(), Refusal> {
        self.inner.wake_on_command()?;
        self.push(Logged::plain(Kind::WakeOnCommand, 0));
        Ok(())
    }

    fn f2_interrupt_setup(&mut self) -> Result<bool, Refusal> {
        let mailbox = self.inner.f2_interrupt_setup()?;
        self.push(Logged::plain(Kind::F2InterruptSetup, u32::from(mailbox)));
        Ok(mailbox)
    }

    fn tune(&mut self, f1_scratch: u32, now: Micros) -> Result<Tune, Refusal> {
        let step = self.inner.tune(f1_scratch, now);
        if let Ok(Tune::Done) = step {
            self.push(Logged::plain(Kind::Tune, f1_scratch));
        }
        step
    }
}
