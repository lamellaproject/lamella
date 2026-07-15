//! The serial-port seam: a UART/COM byte pipe behind a trait the embedder supplies (host =
//! the `serialport` crate over a real COM port; a device = a SERCOM/USART peripheral driver;
//! tests and a browser = an in-memory loopback). Like the file seam ([`crate::fs`]) and unlike
//! the socket seam ([`crate::net`]) every operation is BLOCKING: a read waits up to the
//! configured read timeout, a write drains up to the write timeout, both in bounded time -- so
//! there is no `WouldBlock`, no parking, and no reactor involvement. A blocked read stalls
//! sibling green threads for its (bounded) duration, exactly as a file read does.

/// An open port the backend hands out: an index into the backend's own table, opaque to the
/// interpreter (it just passes the handle back to identify the port).
pub type SerialHandle = u32;

/// Why a serial operation failed. Each variant maps 1:1 to a managed exception type;
/// [`SerialError::code`] is the negative sentinel the intrinsics return across the seam.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SerialError {
    /// No port by that name exists on this host (managed: `IOException` -- "does not exist").
    NotFound,
    /// The port exists but is in use or the caller may not open it (managed:
    /// `UnauthorizedAccessException`).
    AccessDenied,
    /// A write could not make progress within its write timeout (managed: `TimeoutException`).
    /// Reads do NOT use this -- a timed-out read returns `Ok(0)`, matching NETMF's `Read`.
    Timeout,
    /// Any other I/O failure -- the line dropped, the device unplugged, a driver error, or no
    /// backend installed (managed: `IOException`).
    Io,
}

impl SerialError {
    /// The negative sentinel this error crosses the seam as (handles and byte counts are
    /// `>= 0`; `-1` is reserved so the socket seam's `WouldBlock` habit never aliases a real
    /// serial error).
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            SerialError::NotFound => -2,
            SerialError::AccessDenied => -3,
            SerialError::Timeout => -4,
            SerialError::Io => -5,
        }
    }
}

/// The outcome of a serial operation.
pub type SerialResult<T> = Result<T, SerialError>;

/// The parity-checking scheme -- the managed `System.IO.Ports.Parity` values, crossed as their
/// .NET integer codes. The loopback backend ignores framing; a real UART backend applies it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Parity {
    /// No parity bit (.NET 0).
    None,
    /// Odd parity (.NET 1).
    Odd,
    /// Even parity (.NET 2).
    Even,
    /// Mark parity -- the bit is always 1 (.NET 3).
    Mark,
    /// Space parity -- the bit is always 0 (.NET 4).
    Space,
}

impl Parity {
    /// Decodes the managed `Parity` integer; anything unknown is `None`-the-Option (the caller
    /// reports the misuse rather than guessing a scheme).
    #[must_use]
    pub fn from_i32(value: i32) -> Option<Parity> {
        Some(match value {
            0 => Parity::None,
            1 => Parity::Odd,
            2 => Parity::Even,
            3 => Parity::Mark,
            4 => Parity::Space,
            _ => return None,
        })
    }
}

/// The number of stop bits -- the managed `System.IO.Ports.StopBits` values. `None` (0) has no
/// hardware meaning; the managed layer rejects it before the seam, so a backend never sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopBits {
    /// No stop bits -- not a legal line setting (.NET 0; the managed setter throws).
    None,
    /// One stop bit (.NET 1).
    One,
    /// Two stop bits (.NET 2).
    Two,
    /// One and a half stop bits (.NET 3).
    OnePointFive,
}

impl StopBits {
    /// Decodes the managed `StopBits` integer.
    #[must_use]
    pub fn from_i32(value: i32) -> Option<StopBits> {
        Some(match value {
            0 => StopBits::None,
            1 => StopBits::One,
            2 => StopBits::Two,
            3 => StopBits::OnePointFive,
            _ => return None,
        })
    }
}

/// The flow-control protocol -- the managed `System.IO.Ports.Handshake` values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handshake {
    /// No flow control (.NET 0).
    None,
    /// Software XON/XOFF (.NET 1).
    XOnXOff,
    /// Hardware RTS/CTS (.NET 2).
    RequestToSend,
    /// Both hardware RTS/CTS and software XON/XOFF (.NET 3).
    RequestToSendXOnXOff,
}

impl Handshake {
    /// Decodes the managed `Handshake` integer.
    #[must_use]
    pub fn from_i32(value: i32) -> Option<Handshake> {
        Some(match value {
            0 => Handshake::None,
            1 => Handshake::XOnXOff,
            2 => Handshake::RequestToSend,
            3 => Handshake::RequestToSendXOnXOff,
            _ => return None,
        })
    }
}

/// The full line configuration a port opens with. The managed `SerialPort` accumulates these
/// through its property setters (which throw while the port is open) and hands the frozen set to
/// [`SerialBackend::open`]. `data_bits` is 5..=8 (validated managed-side).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SerialConfig {
    /// Bits per second (e.g. 9600, 115200).
    pub baud_rate: i32,
    /// The parity scheme.
    pub parity: Parity,
    /// Data bits per byte (5..=8).
    pub data_bits: i32,
    /// The number of stop bits.
    pub stop_bits: StopBits,
    /// The flow-control protocol.
    pub handshake: Handshake,
}

/// The serial-port seam. `Debug` is a supertrait so the [`crate::interp::Vm`] -- which holds an
/// `Option<Box<dyn SerialBackend>>` -- still derives `Debug`. A device with no serial hardware
/// simply installs no backend: every managed operation then reports [`SerialError::Io`] and the
/// corlib throws a catchable `IOException`, not a trap.
pub trait SerialBackend: core::fmt::Debug {
    /// Opens `port_name` with `config`, returning its handle. The port begins drained
    /// (`bytes_to_read` == `bytes_to_write` == 0).
    fn open(&mut self, port_name: &str, config: &SerialConfig) -> SerialResult<SerialHandle>;

    /// Reads currently-available bytes into `buf`, waiting up to `timeout_ms` for the FIRST byte
    /// (`-1` = infinite, `0` = return at once). Returns the count read; `Ok(0)` means the timeout
    /// elapsed with nothing to read -- NETMF's `SerialPort.Read` returns 0, it does NOT throw.
    /// Never waits once at least one byte is in hand.
    fn read(
        &mut self,
        handle: SerialHandle,
        buf: &mut [u8],
        timeout_ms: i32,
    ) -> SerialResult<usize>;

    /// Writes `buf`, waiting up to `timeout_ms` for room. Returns the bytes accepted (a backend
    /// may take fewer than offered; the managed layer loops the remainder).
    fn write(&mut self, handle: SerialHandle, buf: &[u8], timeout_ms: i32) -> SerialResult<usize>;

    /// The number of bytes in the receive buffer (`SerialPort.BytesToRead`).
    fn bytes_to_read(&mut self, handle: SerialHandle) -> SerialResult<usize>;

    /// The number of bytes still queued for transmission (`SerialPort.BytesToWrite`).
    fn bytes_to_write(&mut self, handle: SerialHandle) -> SerialResult<usize>;

    /// Blocks until the transmit buffer is drained to the line (`SerialPort.Flush`).
    fn flush(&mut self, handle: SerialHandle) -> SerialResult<()>;

    /// Discards the unread receive buffer (`SerialPort.DiscardInBuffer`).
    fn discard_in(&mut self, handle: SerialHandle) -> SerialResult<()>;

    /// Discards the untransmitted send buffer (`SerialPort.DiscardOutBuffer`).
    fn discard_out(&mut self, handle: SerialHandle) -> SerialResult<()>;

    /// Closes the port and releases its handle (idempotent; closing an unknown handle is a
    /// no-op -- Close runs from Dispose and finalizers too).
    fn close(&mut self, handle: SerialHandle);
}

/// A boxed serial backend, as the [`crate::interp::Vm`] stores it.
pub type BoxedSerialBackend = alloc::boxed::Box<dyn SerialBackend>;

/// Decodes the five managed line-configuration integers into a [`SerialConfig`], or `None` if
/// any is out of range (the intrinsic then reports [`SerialError::Io`] rather than guessing).
#[must_use]
pub fn config_from_i32(
    baud_rate: i32,
    parity: i32,
    data_bits: i32,
    stop_bits: i32,
    handshake: i32,
) -> Option<SerialConfig> {
    Some(SerialConfig {
        baud_rate,
        parity: Parity::from_i32(parity)?,
        data_bits,
        stop_bits: StopBits::from_i32(stop_bits)?,
        handshake: Handshake::from_i32(handshake)?,
    })
}
